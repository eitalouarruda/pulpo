import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { periodOf, pickGreeting } from "./greetings";
import { applyI18n, currentLanguage, type Language, LANGUAGES, setLanguage, t, tError } from "./i18n";
import { currentTheme, setTheme, THEMES } from "./theme";
import { setupMusic } from "./music";

interface Service {
  id: string;
  name: string;
  url: string;
  color: string;
  icon: string | null;
  icon_source: IconSource;
  group?: string | null;
  adblock: boolean;
  enabled: boolean;
  dnd?: Dnd;
  /// Tema que la app ve como preferido (como `services::Theme`).
  theme?: AppTheme;
}

type AppTheme = "auto" | "light" | "dark";
const APP_THEMES: AppTheme[] = ["auto", "light", "dark"];

/// No molestar: horarios semanales y silencio manual (como en `services.rs`).
interface Dnd {
  schedules?: DndSchedule[];
  /// Silenciada a mano hasta este instante (segundos Unix).
  until?: number | null;
}

interface DndSchedule {
  /// 0 = lunes … 6 = domingo.
  days: number[];
  /// Minutos desde la medianoche.
  start: number;
  end: number;
}

type IconSource = "web" | "custom" | "letter";

interface Preset {
  name: string;
  url: string;
  color: string;
  adblock: boolean;
}

type BlockerStatus =
  | { state: "loading" }
  | { state: "ready" }
  | { state: "error"; message: string };

const $ = <T extends HTMLElement>(sel: string) => document.querySelector<T>(sel)!;

const nav = $("#services");
const status = $("#status");
const overlay = $("#overlay");
const views = {
  home: $("#home-view"),
  menu: $("#menu-view"),
  disabled: $("#disabled-view"),
  catalog: $("#catalog-view"),
  manage: $("#manage-view"),
  form: $<HTMLFormElement>("#form-view"),
  confirm: $("#confirm-view"),
  switcher: $("#switcher-view"),
  update: $("#update-view"),
  ci: $("#ci-view"),
  wallpaper: $("#wallpaper-view"),
  ciSetup: $("#ci-setup-view"),
  ciToken: $<HTMLFormElement>("#ci-token-view"),
  vault: $<HTMLFormElement>("#vault-view"),
};
const form = {
  title: $("#form-title"),
  name: $<HTMLInputElement>("#f-name"),
  url: $<HTMLInputElement>("#f-url"),
  adblock: $<HTMLInputElement>("#f-adblock"),
  group: $<HTMLInputElement>("#f-group"),
  groups: $("#groups"),
  iconPreview: $("#f-icon"),
  iconFile: $<HTMLInputElement>("#f-icon-file"),
  error: $("#f-error"),
  submit: $<HTMLButtonElement>("#f-submit"),
};

let services: Service[] = [];
/// App de la mitad inferior de la página inicial (null: ninguna).
let homeApp: string | null = null;
let active: string | null = null;
const badges = new Map<string, number>();
/// Apps con no molestar activo ahora (lo decide Rust).
let dndActive = new Set<string>();

// ---------------------------------------------------------------- barra lateral

let lastStatus: BlockerStatus = { state: "loading" };

function showStatus(s: BlockerStatus) {
  lastStatus = s;
  status.dataset.state = s.state;
  status.title =
    s.state === "loading"
      ? t("status.loading")
      : s.state === "ready"
        ? t("status.ready")
        : t("status.error", { message: s.message });
}

function markCurrent() {
  for (const b of nav.querySelectorAll<HTMLElement>(".service")) {
    b.setAttribute("aria-current", String(b.dataset.id === active));
  }
  $("#home").setAttribute("aria-current", String(active === null));
}

/// Apps abiertas, de la más reciente a la más antigua (para Ctrl Ctrl).
let recent: string[] = [];

function select(id: string) {
  active = id;
  recent = [id, ...recent.filter((x) => x !== id)];
  markCurrent();
  invoke("activate_service", { id }).catch(console.error);
  showBase(true);
}

/// Ctrl Ctrl: alterna entre las dos últimas apps (desde la página inicial,
/// vuelve a la última).
function selectPrevious() {
  const target = recent.find((id) => id !== active && services.some((s) => s.id === id));
  if (target) select(target);
}

/// Ctrl+N: la app en la posición N de la barra lateral.
function selectIndex(n: number) {
  const s = services[n - 1];
  if (s) select(s.id);
}

/// Página inicial: ninguna app activa.
function goHome() {
  active = null;
  newGreeting();
  markCurrent();
  invoke("show_home").catch(console.error);
  showBase(true);
}

type View = keyof typeof views;

/// Lo que se ve cuando no hay ningún diálogo: la página inicial, el aviso de
/// app desactivada o (null) la propia app.
function baseView(): View | null {
  if (active === null) return "home";
  return services.find((s) => s.id === active)?.enabled === false ? "disabled" : null;
}

const isBase = (v: View | null) => v === "home" || v === "disabled";

function renderBase(view: View) {
  if (view === "home") return renderHome();
  const s = services.find((s) => s.id === active)!;
  const icon = $("#d-icon");
  fillIcon(icon, s);
  icon.classList.remove("letter");
  if (!s.icon) icon.style.background = s.color;
  $("#d-title").textContent = t("disabled.title", { name: s.name });
}

/// Pone la vista base al día, salvo que haya un diálogo abierto (al cerrarlo
/// se vuelve a ella). `leave`: el usuario fue a otra app o a la página
/// inicial, así que el diálogo se cierra (menos la actualización descargándose).
function showBase(leave = false) {
  let current = overlay.hidden ? null : currentView();
  if (current !== null && !isBase(current)) {
    if (!leave || (updating && current === "update")) return;
    editing = removing = returnTo = null;
    current = "home";
  }
  const base = baseView();
  if (base) {
    renderBase(base);
    openOverlay(base);
  } else if (current !== null) {
    hideOverlay();
  }
}

function renderHome() {
  const home = $("#home-groups");
  const groups = new Map<string | null, Service[]>();
  for (const s of services) {
    const g = s.group ?? null;
    groups.set(g, [...(groups.get(g) ?? []), s]);
  }
  const sections = [...groups].map(([group, list]) => {
    const section = document.createElement("section");
    section.className = "home-group";
    if (group !== null || groups.size > 1) {
      const h = document.createElement("h3");
      h.textContent = group ?? t("home.ungrouped");
      section.append(h);
    }
    const grid = document.createElement("div");
    grid.className = "home-grid";
    for (const s of list) {
      const tile = document.createElement("button");
      tile.type = "button";
      tile.className = "home-app";
      tile.classList.toggle("disabled", !s.enabled);
      tile.title = s.enabled ? s.name : t("service.disabledTitle", { name: s.name });
      const icon = document.createElement("span");
      icon.className = "service home-icon";
      fillIcon(icon, s);
      const badge = document.createElement("span");
      badge.className = "badge";
      const count = badges.get(s.id) ?? 0;
      badge.textContent = count > 0 ? String(count) : "";
      icon.append(badge);
      if (dndActive.has(s.id)) icon.append(dndMark());
      const name = document.createElement("span");
      name.className = "home-name";
      name.textContent = s.name;
      tile.append(icon, name);
      tile.addEventListener("click", () => select(s.id));
      tile.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        openMenu(s, e.clientY, e.clientX);
      });
      grid.append(tile);
    }
    section.append(grid);
    return section;
  });
  if (sections.length === 0) {
    const empty = document.createElement("p");
    empty.className = "home-empty";
    empty.textContent = t("home.empty");
    sections.push(empty as unknown as HTMLElement);
  }
  home.replaceChildren(...sections);
}

// ---------------------------------------------------------------- reloj y saludo

let greeting = "";
let greetingPeriod = "";

/// Saludo nuevo en cada visita a la página inicial (y al cambiar de periodo).
function newGreeting() {
  const now = new Date();
  greetingPeriod = periodOf(now);
  greeting = pickGreeting(currentLanguage(), periodOf(now), greeting);
  renderClock();
}

function renderClock() {
  const now = new Date();
  if (periodOf(now) !== greetingPeriod) return newGreeting();
  const lang = currentLanguage();
  const time = $("#home-time");
  time.textContent = now.toLocaleTimeString(lang, { hour: "2-digit", minute: "2-digit" });
  time.setAttribute("datetime", now.toISOString());
  const date = now.toLocaleDateString(lang, { weekday: "long", day: "numeric", month: "long" });
  $("#home-date").textContent = date.charAt(0).toLocaleUpperCase(lang) + date.slice(1);
  $("#home-greeting").textContent = greeting;
}

/// Al empezar cada minuto, aunque no se vea (es barato).
function tickClock() {
  renderClock();
  setTimeout(tickClock, 60_000 - (Date.now() % 60_000) + 50);
}

/// Grupos plegados (por nombre). Es solo de la vista: se recuerda aquí.
const collapsed = new Set<string>(loadCollapsed());

function loadCollapsed(): string[] {
  try {
    return JSON.parse(localStorage.getItem("pulpo.collapsed") ?? "[]");
  } catch {
    return [];
  }
}

function toggleGroup(group: string) {
  if (collapsed.has(group)) collapsed.delete(group);
  else collapsed.add(group);
  try {
    localStorage.setItem("pulpo.collapsed", JSON.stringify([...collapsed]));
  } catch {
    /* sin almacenamiento: no se recuerda */
  }
  renderSidebar();
}

function serviceButton(s: Service, index: number) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "service";
  button.dataset.id = s.id;
  button.draggable = true;
  button.title = [
    s.name,
    index < 9 ? `Ctrl+${index + 1}` : "",
    s.enabled ? "" : t("service.disabled"),
    dndActive.has(s.id) ? t("dnd.active") : "",
  ]
    .filter(Boolean)
    .join(" · ");
  button.setAttribute("aria-current", String(s.id === active));
  button.classList.toggle("disabled", !s.enabled);
  button.style.setProperty("--color", s.color);
  fillIcon(button, s);

  const badge = document.createElement("span");
  badge.className = "badge";
  const count = badges.get(s.id) ?? 0;
  badge.textContent = count > 0 ? String(count) : "";
  button.append(badge);
  if (dndActive.has(s.id)) button.append(dndMark());

  button.addEventListener("click", () => select(s.id));
  button.addEventListener("auxclick", (e) => {
    if (e.button === 1) invoke("reload_service", { id: s.id }).catch(console.error);
  });
  button.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    openMenu(s, button.getBoundingClientRect().top);
  });
  return button;
}

/// Luna en la esquina del icono de una app en no molestar.
function dndMark() {
  const mark = document.createElement("span");
  mark.className = "dnd-mark";
  mark.textContent = "☾";
  mark.title = t("dnd.active");
  return mark;
}

/// Pone el icono (imagen o inicial con color) dentro de `el`.
function fillIcon(el: HTMLElement, s: Pick<Service, "name" | "color" | "icon">) {
  el.replaceChildren();
  el.classList.toggle("letter", !s.icon);
  el.style.setProperty("--color", s.color);
  if (s.icon) {
    const img = document.createElement("img");
    img.src = s.icon;
    img.alt = "";
    img.draggable = false;
    el.append(img);
  } else {
    el.textContent = s.name.trim()[0]?.toUpperCase() ?? "?";
  }
}

function renderSidebar() {
  const items: HTMLElement[] = [];
  let group: string | null = null;
  for (const [i, s] of services.entries()) {
    const g = s.group ?? null;
    if (g !== null && g !== group) {
      const header = document.createElement("button");
      header.type = "button";
      header.className = "group";
      header.dataset.group = g;
      header.title = t(collapsed.has(g) ? "group.toggle.expand" : "group.toggle.collapse", { group: g });
      header.setAttribute("aria-expanded", String(!collapsed.has(g)));
      header.textContent = g;
      const unread = services
        .filter((x) => x.group === g)
        .reduce((n, x) => n + (badges.get(x.id) ?? 0), 0);
      if (collapsed.has(g) && unread > 0) header.dataset.unread = String(unread);
      header.addEventListener("click", () => toggleGroup(g));
      items.push(header);
    }
    group = g;
    // En un grupo plegado solo se ve la app activa.
    if (g !== null && collapsed.has(g) && s.id !== active) continue;
    items.push(serviceButton(s, i));
  }
  nav.replaceChildren(...items);
}

// Reordenar arrastrando.
let dragged: HTMLElement | null = null;
nav.addEventListener("dragstart", (e) => {
  dragged = (e.target as HTMLElement).closest(".service");
  dragged?.classList.add("dragging");
});
nav.addEventListener("dragover", (e) => {
  e.preventDefault();
  const over = (e.target as HTMLElement).closest<HTMLElement>(".service, .group");
  if (!dragged || !over || over === dragged) return;
  // Soltar sobre el título de un grupo mete la app al principio de ese grupo.
  if (over.classList.contains("group")) return over.after(dragged);
  const { top, height } = over.getBoundingClientRect();
  over[e.clientY < top + height / 2 ? "before" : "after"](dragged);
});
nav.addEventListener("dragend", () => {
  dragged?.classList.remove("dragging");
  dragged = null;
  // El grupo de cada app es el del último título que tiene encima.
  const order: { id: string; group: string | null }[] = [];
  let group: string | null = null;
  for (const el of nav.children as HTMLCollectionOf<HTMLElement>) {
    if (el.classList.contains("group")) group = el.dataset.group!;
    else order.push({ id: el.dataset.id!, group });
  }
  // Las apps de grupos plegados no están en la barra: siguen igual.
  for (const s of services) {
    if (!order.some((o) => o.id === s.id)) order.push({ id: s.id, group: s.group ?? null });
  }
  const before = services.map((s) => `${s.id}:${s.group ?? ""}`).join();
  const after = order.map((o) => `${o.id}:${o.group ?? ""}`).join();
  if (before !== after) invoke("reorder_services", { order }).catch(console.error);
});

// ---------------------------------------------------------------- diálogos

let editing: string | null = null;
/// Vista a la que vuelve el formulario o la confirmación al cerrarse.
let returnTo: View | null = null;
let removing: string | null = null;

/// Avisa al backend si se ve la página inicial sin diálogos: solo entonces
/// se muestra la app de abajo (un diálogo ocupa toda la ventana).
let paneVisible = false;
function syncHomePane() {
  const visible = !overlay.hidden && currentView() === "home";
  document.body.classList.toggle("split", homeApp !== null && visible);
  if (visible === paneVisible) return;
  paneVisible = visible;
  invoke("set_home_pane_visible", { visible }).catch(console.error);
}

/// Modales de CI: se abren encima de la página de CI, que sigue visible.
const isCiModal = (v: View | null) => v === "ciSetup" || v === "ciToken";

function openOverlay(view: View) {
  for (const [name, el] of Object.entries(views)) {
    el.hidden = name !== view && !(name === "ci" && isCiModal(view));
  }
  if (!overlay.hidden) syncHomePane();
  overlay.classList.toggle("menu-mode", view === "menu");
  overlay.classList.toggle("home-mode", view === "home");
  overlay.classList.toggle("wallpaper-mode", view === "wallpaper");
  // Páginas completas (CI, fondo de pantalla), no tarjetas.
  overlay.classList.toggle("page-mode", view === "ci" || isCiModal(view));
  overlay.classList.toggle("ci-modal-open", isCiModal(view));
  if (!overlay.hidden) return;
  invoke("set_overlay", { open: true })
    .catch(console.error)
    .finally(() => {
      overlay.hidden = false;
      document.body.classList.add("overlay-open");
      syncHomePane();
      if (!isBase(view)) {
        (views[view].querySelector("input, button:not(.ghost)") as HTMLElement | null)?.focus();
      }
    });
}

function hideOverlay() {
  overlay.hidden = true;
  syncHomePane();
  document.body.classList.remove("overlay-open");
  invoke("set_overlay", { open: false }).catch(console.error);
}

/// Cierra el diálogo abierto: vuelve a la vista de la que vino, a la vista
/// base, o deja ver la app.
function closeOverlay() {
  if (overlay.hidden) return;
  // Descargando la actualización: al terminar Pulpo se reinicia solo.
  if (updating && currentView() === "update") return;
  const current = currentView();
  editing = removing = null;
  // Añadir token vuelve a la configuración, que se rehace con lo nuevo.
  if (current === "ciToken") return openCiSetup();
  if (returnTo && returnTo !== current) {
    const back = returnTo;
    returnTo = null;
    if (back === "manage") renderManage();
    return openOverlay(back);
  }
  returnTo = null;
  const base = baseView();
  if (base) {
    if (current !== base) {
      renderBase(base);
      openOverlay(base);
    }
  } else {
    hideOverlay();
  }
}

function currentView(): View | null {
  // Al revés: con un modal de CI abierto, la página de CI también se ve.
  for (const [name, el] of Object.entries(views).reverse()) {
    if (!el.hidden) return name as View;
  }
  return null;
}

function renderHomeAppChoice() {
  const box = $("#home-app");
  const choice = (id: string | null, label: string) => {
    const b = document.createElement("button");
    b.type = "button";
    b.role = "radio";
    b.className = "ghost-btn";
    b.textContent = label;
    b.setAttribute("aria-checked", String(id === homeApp));
    b.addEventListener("click", () => setHomeApp(id));
    return b;
  };
  box.replaceChildren(
    choice(null, t("manage.homeAppNone")),
    ...services.filter((s) => s.enabled).map((s) => choice(s.id, s.name)),
  );
}

function setHomeApp(id: string | null) {
  homeApp = id;
  renderHomeAppChoice();
  syncHomePane();
  invoke("set_home_app", { id }).catch(console.error);
}

function renderManage() {
  renderHomeAppChoice();
  renderLinks();
  const list = $("#manage-list");
  list.replaceChildren(
    ...services.map((s) => {
      const li = document.createElement("li");
      li.className = s.enabled ? "" : "disabled";
      const icon = document.createElement("span");
      icon.className = "m-icon";
      if (s.icon) {
        const img = document.createElement("img");
        img.src = s.icon;
        img.alt = "";
        icon.append(img);
      } else {
        icon.style.background = s.color;
        icon.textContent = s.name.trim()[0]?.toUpperCase() ?? "?";
      }
      const text = document.createElement("span");
      text.className = "m-text";
      const name = document.createElement("strong");
      name.textContent = s.name;
      const meta = document.createElement("small");
      meta.textContent = [
        new URL(s.url).host,
        s.group ? t("manage.group", { group: s.group }) : "",
        t(s.adblock ? "manage.withAdblock" : "manage.withoutAdblock"),
        s.enabled ? "" : t("service.disabled"),
      ]
        .filter(Boolean)
        .join(" · ");
      text.append(name, meta);

      const button = (label: string, cls: string, onClick: () => void) => {
        const b = document.createElement("button");
        b.type = "button";
        b.className = cls;
        b.textContent = label;
        b.addEventListener("click", onClick);
        return b;
      };
      const sameGroup = services.filter((x) => (x.group ?? null) === (s.group ?? null));
      const pos = sameGroup.indexOf(s);
      const up = button("↑", "small icon-btn", () => move(s.id, -1));
      const down = button("↓", "small icon-btn", () => move(s.id, 1));
      up.title = t("manage.up");
      down.title = t("manage.down");
      up.disabled = pos === 0;
      down.disabled = pos === sameGroup.length - 1;
      li.append(
        up,
        down,
        icon,
        text,
        button(t("action.edit"), "small", () => {
          returnTo = "manage";
          openForm(s, s.id);
        }),
        button(t(s.enabled ? "action.disable" : "action.enable"), "small ghost-btn", () => {
          invoke("set_service_enabled", { id: s.id, enabled: !s.enabled }).catch(console.error);
        }),
        button(t("action.remove"), "small danger-btn", () => {
          returnTo = "manage";
          askRemove(s.id);
        }),
      );
      return li;
    }),
  );
}

/// Sube o baja una app dentro de su grupo.
function move(id: string, delta: -1 | 1) {
  const order = services.map((s) => ({ id: s.id, group: s.group ?? null }));
  const i = order.findIndex((o) => o.id === id);
  const j = i + delta;
  if (j < 0 || j >= order.length || order[j].group !== order[i].group) return;
  [order[i], order[j]] = [order[j], order[i]];
  invoke("reorder_services", { order }).catch(console.error);
}

/// Menú del botón derecho. Lo dibuja la barra lateral (estirada a toda la
/// ventana mientras se ve): el menú nativo no aparece con CEF en Linux.
function openMenu(s: Service, top: number, left?: number) {
  const menu = views.menu;
  type Opts = { checked?: boolean; danger?: boolean; disabled?: boolean; dialog?: boolean };
  const item = (label: string, action: () => void, opts: Opts = {}) => {
    const b = document.createElement("button");
    b.type = "button";
    b.role = opts.checked === undefined ? "menuitem" : "menuitemcheckbox";
    b.className = opts.danger ? "danger" : "";
    b.disabled = !!opts.disabled;
    if (opts.checked !== undefined) b.setAttribute("aria-checked", String(opts.checked));
    b.innerHTML = `<span class="mark"></span><span></span>`;
    b.lastElementChild!.textContent = label;
    b.addEventListener("click", () => {
      // Las que abren un diálogo cambian de vista sin cerrar la superposición.
      if (!opts.dialog) closeOverlay();
      action();
    });
    return b;
  };
  const sep = () => document.createElement("hr");
  const title = document.createElement("div");
  title.className = "menu-title";
  title.textContent = s.name;
  menu.replaceChildren(
    title,
    item(t("action.reload"), () => invoke("reload_service", { id: s.id }).catch(console.error), {
      disabled: !s.enabled,
    }),
    item(
      t("action.adblock"),
      () =>
        invoke("update_service", {
          id: s.id,
          name: s.name,
          url: s.url,
          adblock: !s.adblock,
          group: s.group ?? null,
          theme: s.theme ?? "auto",
        }).catch(
          console.error,
        ),
      { checked: s.adblock },
    ),
    item(t("action.homeApp"), () => setHomeApp(homeApp === s.id ? null : s.id), {
      checked: homeApp === s.id,
      disabled: !s.enabled,
    }),
    sep(),
    ...dndItems(s, item),
    sep(),
    item(t("action.editMenu"), () => openForm(s, s.id), { dialog: true }),
    item(t(s.enabled ? "action.disable" : "action.enable"), () =>
      invoke("set_service_enabled", { id: s.id, enabled: !s.enabled }).catch(console.error),
    ),
    item(t("action.removeMenu"), () => askRemove(s.id), { danger: true, dialog: true }),
  );
  menu.style.left = left === undefined ? "" : `${Math.min(left, window.innerWidth - 230)}px`;
  openOverlay("menu");
  // Ya visible, con su altura real: que no se salga por abajo.
  menu.style.top = `${Math.max(8, Math.min(top, window.innerHeight - menu.offsetHeight - 8))}px`;
}

/// Opciones de no molestar del menú: silenciar a mano o reactivar.
function dndItems(
  s: Service,
  item: (label: string, action: () => void, opts?: { disabled?: boolean }) => HTMLElement,
) {
  const silence = (until: number | null) =>
    invoke("silence_service", { id: s.id, until }).catch(console.error);
  const now = Date.now() / 1000;
  const until = s.dnd?.until ?? null;
  if (until !== null && until > now) {
    const label =
      until >= FOREVER ? t("dnd.resume") : t("dnd.resumeUntil", { time: formatUntil(until) });
    return [item(label, () => silence(null))];
  }
  const items = [
    item(t("dnd.hour"), () => silence(Math.round(now) + 3600)),
    item(t("dnd.tomorrow"), () => silence(nextMorning())),
    item(t("dnd.forever"), () => silence(FOREVER)),
  ];
  // En horario de no molestar: se avisa (el horario se cambia en Editar).
  if (dndActive.has(s.id)) items.unshift(item(t("dnd.scheduled"), () => {}, { disabled: true }));
  return items;
}

/// "Hasta que la reactive": 31/12/9999.
const FOREVER = 253402300799;

/// Las próximas 8:00 (segundos Unix).
function nextMorning() {
  const d = new Date();
  if (d.getHours() >= 8) d.setDate(d.getDate() + 1);
  d.setHours(8, 0, 0, 0);
  return Math.round(d.getTime() / 1000);
}

/// La hora (y el día si no es hoy) de un instante.
function formatUntil(until: number) {
  const d = new Date(until * 1000);
  const sameDay = d.toDateString() === new Date().toDateString();
  return d.toLocaleString(currentLanguage(), {
    weekday: sameDay ? undefined : "short",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function askRemove(id: string) {
  const s = services.find((s) => s.id === id);
  if (!s) return;
  removing = id;
  $("#confirm-title").textContent = t("confirm.title", { name: s.name });
  openOverlay("confirm");
}

$("#d-enable").addEventListener("click", () => {
  if (active) invoke("set_service_enabled", { id: active, enabled: true }).catch(console.error);
});

$("#manage").addEventListener("click", () => {
  renderManage();
  openOverlay("manage");
  invoke<LinkRule[]>("get_link_rules")
    .then((rules) => {
      linkRules = rules;
      renderLinks();
    })
    .catch(console.error);
});

// ---------------------------------------------------------------- enlaces

/// Espejo de `links::LinkRule`. `app` sin valor: el navegador del sistema.
interface LinkRule {
  pattern: string;
  app: string | null;
}

let linkRules: LinkRule[] = [];
const linksMenu = $("#links-menu");

/// Se guarda tal cual; Rust descarta las vacías al guardar, pero aquí se
/// quedan mientras se escriben.
const saveLinks = () => invoke("set_link_rules", { rules: linkRules }).catch(console.error);

const linkAppName = (id: string | null) =>
  (id && services.find((s) => s.id === id)?.name) || t("links.browser");

function renderLinks() {
  closeLinksMenu();
  $("#links-list").replaceChildren(
    ...linkRules.map((rule, i) => {
      const row = document.createElement("div");
      row.className = "link-row";
      const input = document.createElement("input");
      input.type = "text";
      input.spellcheck = false;
      input.placeholder = t("links.placeholder");
      input.value = rule.pattern;
      input.addEventListener("input", () => (rule.pattern = input.value));
      input.addEventListener("change", saveLinks);
      const arrow = document.createElement("span");
      arrow.textContent = "→";
      const dest = document.createElement("button");
      dest.type = "button";
      dest.className = "ghost-btn link-dest";
      dest.textContent = linkAppName(rule.app);
      dest.addEventListener("click", () => openLinksMenu(dest, rule));
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "ghost-btn icon-btn";
      remove.textContent = "×";
      remove.title = t("links.remove");
      remove.addEventListener("click", () => {
        linkRules.splice(i, 1);
        renderLinks();
        saveLinks();
      });
      row.append(input, arrow, dest, remove);
      return row;
    }),
  );
}

function openLinksMenu(anchor: HTMLElement, rule: LinkRule) {
  const item = (id: string | null) => {
    const b = document.createElement("button");
    b.type = "button";
    b.role = "menuitemradio";
    b.setAttribute("aria-checked", String(id === rule.app));
    b.innerHTML = `<span class="mark"></span><span></span>`;
    b.lastElementChild!.textContent = linkAppName(id);
    b.addEventListener("click", () => {
      rule.app = id;
      anchor.textContent = linkAppName(id);
      closeLinksMenu();
      saveLinks();
    });
    return b;
  };
  linksMenu.replaceChildren(
    item(null),
    document.createElement("hr"),
    ...services.filter((s) => s.enabled).map((s) => item(s.id)),
  );
  linksMenu.hidden = false;
  const r = anchor.getBoundingClientRect();
  linksMenu.style.left = `${Math.max(8, Math.min(r.left, window.innerWidth - linksMenu.offsetWidth - 8))}px`;
  // Debajo del botón, o encima si no cabe.
  const below = r.bottom + 4;
  linksMenu.style.top = `${below + linksMenu.offsetHeight > window.innerHeight - 8 ? Math.max(8, r.top - linksMenu.offsetHeight - 4) : below}px`;
}

function closeLinksMenu() {
  linksMenu.hidden = true;
}

$("#links-add").addEventListener("click", () => {
  linkRules.push({ pattern: "", app: null });
  renderLinks();
  $<HTMLInputElement>("#links-list .link-row:last-child input").focus();
});
document.addEventListener("pointerdown", (e) => {
  const target = e.target as HTMLElement;
  if (!linksMenu.hidden && !linksMenu.contains(target) && !target.closest(".link-dest")) closeLinksMenu();
});
// Esc con la lista abierta solo la cierra a ella, no el diálogo.
window.addEventListener(
  "keydown",
  (e) => {
    if (e.key === "Escape" && !linksMenu.hidden) {
      e.stopImmediatePropagation();
      closeLinksMenu();
    }
  },
  true,
);

/// Icono elegido en el formulario (se aplica al guardar).
let iconChoice: { source: IconSource; icon: string | null } = { source: "web", icon: null };
let formColor = "#8e4ec6";

interface FormInit {
  name: string;
  url: string;
  adblock: boolean;
  color?: string;
  group?: string | null;
  icon?: string | null;
  icon_source?: IconSource;
  dnd?: Dnd;
  theme?: AppTheme;
}

let formTheme: AppTheme = "auto";

function renderFormTheme() {
  $("#f-theme").replaceChildren(
    ...APP_THEMES.map((value) => {
      const b = document.createElement("button");
      b.type = "button";
      b.role = "radio";
      b.className = "ghost-btn";
      b.textContent = t(`form.theme.${value}`);
      b.setAttribute("aria-checked", String(value === formTheme));
      b.addEventListener("click", () => {
        formTheme = value;
        renderFormTheme();
      });
      return b;
    }),
  );
}

// Sugerencias del campo de grupo: los grupos que ya existen y contienen lo escrito.
let groupIndex = -1;

function groupSuggestions(): string[] {
  const typed = form.group.value.trim().toLowerCase();
  const all = [...new Set(services.map((s) => s.group).filter((g): g is string => !!g))];
  return all.filter((g) => g.toLowerCase().includes(typed) && g.toLowerCase() !== typed);
}

function renderGroupSuggestions() {
  const list = groupSuggestions();
  groupIndex = Math.min(groupIndex, list.length - 1);
  form.groups.hidden = list.length === 0;
  form.groups.replaceChildren(
    ...list.map((g, i) => {
      const item = document.createElement("div");
      item.role = "option";
      item.textContent = g;
      item.setAttribute("aria-selected", String(i === groupIndex));
      // mousedown y no click: así el campo no pierde el foco antes de elegir.
      item.addEventListener("mousedown", (e) => {
        e.preventDefault();
        pickGroup(g);
      });
      item.addEventListener("mousemove", () => {
        if (groupIndex === i) return;
        groupIndex = i;
        renderGroupSuggestions();
      });
      return item;
    }),
  );
}

function pickGroup(group: string) {
  form.group.value = group;
  groupIndex = -1;
  form.groups.hidden = true;
}

form.group.addEventListener("focus", () => {
  groupIndex = -1;
  renderGroupSuggestions();
});
form.group.addEventListener("input", () => {
  groupIndex = -1;
  renderGroupSuggestions();
});
form.group.addEventListener("blur", () => (form.groups.hidden = true));
form.group.addEventListener("keydown", (e) => {
  if (form.groups.hidden) return;
  const list = groupSuggestions();
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const step = e.key === "ArrowDown" ? 1 : -1;
    groupIndex = (groupIndex + step + list.length + 1) % (list.length + 1);
    if (groupIndex === list.length) groupIndex = -1;
    renderGroupSuggestions();
  } else if (e.key === "Enter" && groupIndex >= 0) {
    e.preventDefault();
    pickGroup(list[groupIndex]);
  } else if (e.key === "Escape") {
    // Cierra solo la lista, no el formulario.
    e.stopPropagation();
    form.groups.hidden = true;
  }
});

function openForm(init: FormInit, edit: string | null) {
  editing = edit;
  form.title.textContent = t(edit ? "form.edit" : "form.new");
  form.submit.textContent = t(edit ? "action.save" : "action.add");
  form.name.value = init.name;
  form.url.value = init.url;
  form.adblock.checked = init.adblock;
  form.group.value = init.group ?? "";
  form.groups.hidden = true;
  formColor = init.color ?? "#8e4ec6";
  formSchedules = structuredClone(init.dnd?.schedules ?? []);
  renderSchedules();
  formTheme = init.theme ?? "auto";
  renderFormTheme();
  iconChoice = { source: init.icon_source ?? "web", icon: init.icon ?? null };
  renderIconPreview();
  form.error.textContent = "";
  openOverlay("form");
  (init.url ? form.name : form.url).focus();
}

function renderIconPreview() {
  const name = form.name.value || form.url.value.replace(/^https?:\/\//, "") || "?";
  fillIcon(form.iconPreview, { name, color: formColor, icon: iconChoice.icon });
  for (const b of document.querySelectorAll<HTMLButtonElement>("[data-icon]")) {
    b.setAttribute("aria-pressed", String(b.dataset.icon === iconChoice.source));
  }
}

form.name.addEventListener("input", renderIconPreview);

/// Horarios de no molestar del formulario (se guardan al guardar).
let formSchedules: DndSchedule[] = [];

const toTime = (minutes: number) =>
  `${String(Math.floor(minutes / 60)).padStart(2, "0")}:${String(minutes % 60).padStart(2, "0")}`;
const fromTime = (value: string) => {
  const [h, m] = value.split(":").map(Number);
  return Number.isFinite(h) && Number.isFinite(m) ? h * 60 + m : null;
};

function renderSchedules() {
  const letters = [...t("dnd.days")];
  $("#f-dnd-list").replaceChildren(
    ...formSchedules.map((sched, i) => {
      const row = document.createElement("div");
      row.className = "dnd-row";
      const days = document.createElement("div");
      days.className = "dnd-days";
      for (const [day, letter] of letters.entries()) {
        const b = document.createElement("button");
        b.type = "button";
        b.textContent = letter;
        b.title = new Date(2024, 0, 1 + day).toLocaleDateString(currentLanguage(), { weekday: "long" });
        b.setAttribute("aria-pressed", String(sched.days.includes(day)));
        b.addEventListener("click", () => {
          sched.days = sched.days.includes(day)
            ? sched.days.filter((d) => d !== day)
            : [...sched.days, day].sort();
          b.setAttribute("aria-pressed", String(sched.days.includes(day)));
        });
        days.append(b);
      }
      const time = (key: "start" | "end") => {
        const input = document.createElement("input");
        input.type = "time";
        input.step = "300";
        input.required = true;
        input.value = toTime(sched[key]);
        input.addEventListener("change", () => {
          const minutes = fromTime(input.value);
          if (minutes !== null) sched[key] = minutes;
        });
        return input;
      };
      const to = document.createElement("span");
      to.textContent = t("dnd.to");
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "ghost-btn icon-btn";
      remove.textContent = "×";
      remove.title = t("dnd.remove");
      remove.addEventListener("click", () => {
        formSchedules.splice(i, 1);
        renderSchedules();
      });
      row.append(days, time("start"), to, time("end"), remove);
      return row;
    }),
  );
}

$("#f-dnd-add").addEventListener("click", () => {
  // Lo más común: de noche, de lunes a viernes.
  formSchedules.push({ days: [0, 1, 2, 3, 4], start: 22 * 60, end: 8 * 60 });
  renderSchedules();
});

/// Reduce la imagen elegida a 128×128 (PNG), para que `services.json` no crezca.
function loadIcon(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const img = new Image();
    img.onload = () => {
      const size = 128;
      const canvas = document.createElement("canvas");
      canvas.width = canvas.height = size;
      const ctx = canvas.getContext("2d")!;
      const scale = Math.min(size / (img.naturalWidth || size), size / (img.naturalHeight || size));
      const w = (img.naturalWidth || size) * scale;
      const h = (img.naturalHeight || size) * scale;
      ctx.drawImage(img, (size - w) / 2, (size - h) / 2, w, h);
      URL.revokeObjectURL(url);
      resolve(canvas.toDataURL("image/png"));
    };
    img.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error(t("form.imageError")));
    };
    img.src = url;
  });
}

for (const b of document.querySelectorAll<HTMLButtonElement>("[data-icon]")) {
  b.addEventListener("click", () => {
    const source = b.dataset.icon as IconSource;
    if (source === "custom") return form.iconFile.click();
    // "De la web": la vista previa queda con la inicial hasta que se descargue.
    iconChoice = { source, icon: null };
    renderIconPreview();
  });
}
form.iconFile.addEventListener("change", async () => {
  const file = form.iconFile.files?.[0];
  form.iconFile.value = "";
  if (!file) return;
  try {
    iconChoice = { source: "custom", icon: await loadIcon(file) };
    form.error.textContent = "";
  } catch (err) {
    form.error.textContent = String(err instanceof Error ? err.message : err);
  }
  renderIconPreview();
});

const catalog = await invoke<Preset[]>("catalog");
const grid = $("#catalog");
const search = $<HTMLInputElement>("#search");

function renderCatalog() {
  const q = search.value.trim().toLowerCase();
  const tiles = catalog
    .filter((p) => p.name.toLowerCase().includes(q))
    .map((p) => {
      const tile = document.createElement("button");
      tile.type = "button";
      tile.className = "tile";
      tile.innerHTML = `<span class="tile-icon"></span><span class="tile-name"></span>`;
      tile.querySelector<HTMLElement>(".tile-icon")!.style.background = p.color;
      tile.querySelector(".tile-icon")!.textContent = p.name[0];
      tile.querySelector(".tile-name")!.textContent = p.name;
      tile.addEventListener("click", () => openForm(p, null));
      return tile;
    });
  const custom = document.createElement("button");
  custom.type = "button";
  custom.className = "tile custom";
  custom.innerHTML = `<span class="tile-icon">+</span><span class="tile-name"></span>`;
  custom.querySelector(".tile-name")!.textContent = t("catalog.custom");
  custom.addEventListener("click", () =>
    openForm({ name: search.value.trim(), url: "", adblock: true }, null),
  );
  grid.replaceChildren(...tiles, custom);
}

$("#add").addEventListener("click", () => {
  search.value = "";
  renderCatalog();
  openOverlay("catalog");
  search.focus();
});
search.addEventListener("input", renderCatalog);
search.addEventListener("keydown", (e) => {
  if (e.key === "Enter") grid.querySelector<HTMLButtonElement>(".tile")?.click();
});

views.form.addEventListener("submit", async (e) => {
  e.preventDefault();
  const args = {
    name: form.name.value,
    url: form.url.value,
    adblock: form.adblock.checked,
    group: form.group.value.trim() || null,
    theme: formTheme,
  };
  form.submit.disabled = true;
  try {
    let id = editing;
    const before = services.find((s) => s.id === editing);
    if (id) {
      await invoke("update_service", { id, ...args });
    } else {
      id = (await invoke<Service>("add_service", args)).id;
      active = id;
    }
    const schedules = formSchedules.filter((sched) => sched.days.length > 0);
    if (JSON.stringify(schedules) !== JSON.stringify(before?.dnd?.schedules ?? [])) {
      await invoke("set_dnd_schedules", { id, schedules });
    }
    const iconChanged =
      !before ||
      before.icon_source !== iconChoice.source ||
      (iconChoice.source === "custom" && before.icon !== iconChoice.icon);
    if (iconChanged && !(iconChoice.source === "web" && !before)) {
      await invoke("set_service_icon", { id, ...iconChoice });
    }
    closeOverlay();
  } catch (err) {
    form.error.textContent = tError(err);
  } finally {
    form.submit.disabled = false;
  }
});

$("#confirm-ok").addEventListener("click", () => {
  if (removing) invoke("remove_service", { id: removing }).catch(console.error);
  closeOverlay();
});

for (const b of overlay.querySelectorAll("[data-close]")) {
  b.addEventListener("click", closeOverlay);
}
overlay.addEventListener("click", (e) => {
  if (e.target === overlay && !isBase(currentView())) closeOverlay();
});
// Con el menú abierto, un clic en la barra lateral lo cierra (y sigue su curso).
$("#sidebar").addEventListener("mousedown", () => {
  if (currentView() === "menu" && !overlay.hidden) closeOverlay();
});

// ---------------------------------------------------------------- tamaño de la app de la página inicial

/// Parte de la altura que ocupa la app de abajo (como en Rust: 0,15 a 0,92).
let paneSize = 0.5;
const clampPane = (size: number) => Math.min(0.92, Math.max(0.15, size));

/// Por debajo de esta altura (px CSS) la página inicial deja solo los iconos.
const TIGHT_HOME = 130;

function applyPaneSize(size: number) {
  paneSize = size;
  document.body.style.setProperty("--pane", String(size));
  document.body.classList.toggle("pane-tight", window.innerHeight * (1 - size) < TIGHT_HOME);
}
window.addEventListener("resize", () => applyPaneSize(paneSize));

/// Mueve el divisor y recoloca la app; `save` lo guarda en los ajustes.
function setPaneSize(size: number, save: boolean) {
  applyPaneSize(clampPane(size));
  return invoke<number>("set_home_pane_size", { size: paneSize, save })
    .then(applyPaneSize)
    .catch(console.error);
}

// Arrastrar el divisor. Con el botón pulsado los movimientos siguen llegando
// aquí aunque el puntero pase por encima de la app (captura del puntero).
const paneHandle = $("#pane-handle");
let paneDrag: { pending: number | null } | null = null;
paneHandle.addEventListener("pointerdown", (e) => {
  if (e.button !== 0) return;
  e.preventDefault();
  paneHandle.setPointerCapture(e.pointerId);
  paneHandle.classList.add("dragging");
  document.body.classList.add("resizing-pane");
  paneDrag = { pending: null };
});
paneHandle.addEventListener("pointermove", (e) => {
  if (!paneDrag) return;
  const first = paneDrag.pending === null;
  paneDrag.pending = clampPane(1 - e.clientY / window.innerHeight);
  applyPaneSize(paneDrag.pending);
  // Una recolocación de la app por cuadro como mucho.
  if (first) {
    requestAnimationFrame(() => {
      if (paneDrag?.pending == null) return;
      const size = paneDrag.pending;
      paneDrag.pending = null;
      setPaneSize(size, false);
    });
  }
});
const endPaneDrag = () => {
  if (!paneDrag) return;
  paneDrag = null;
  paneHandle.classList.remove("dragging");
  document.body.classList.remove("resizing-pane");
  setPaneSize(paneSize, true);
};
paneHandle.addEventListener("pointerup", endPaneDrag);
paneHandle.addEventListener("pointercancel", endPaneDrag);
paneHandle.addEventListener("dblclick", () => setPaneSize(0.5, true));

// ---------------------------------------------------------------- selector de apps

const switcher = {
  search: $<HTMLInputElement>("#switcher-search"),
  list: $("#switcher-list"),
  empty: $("#switcher-empty"),
};
let switcherItems: Service[] = [];
let switcherIndex = 0;

/// Sin tildes ni mayúsculas: "camara" encuentra "Cámara".
const fold = (text: string) => text.normalize("NFD").replace(/\p{M}/gu, "").toLowerCase();

/// Ctrl+Espacio: las apps activadas, filtradas por nombre. Primero las usadas
/// más recientemente; las que empiezan por lo escrito, antes que el resto.
function openSwitcher() {
  const view = overlay.hidden ? null : currentView();
  if (view === "switcher") return moveSwitcher(1);
  if (view !== null && !isBase(view)) return;
  switcher.search.value = "";
  renderSwitcher();
  openOverlay("switcher");
  switcher.search.focus();
}

function renderSwitcher() {
  const q = fold(switcher.search.value.trim());
  const enabled = services.filter((s) => s.enabled);
  const byRecent = [
    ...recent.flatMap((id) => enabled.filter((s) => s.id === id)),
    ...enabled.filter((s) => !recent.includes(s.id)),
  ];
  const matches = byRecent.filter((s) => fold(s.name).includes(q));
  switcherItems = [
    ...matches.filter((s) => fold(s.name).startsWith(q)),
    ...matches.filter((s) => !fold(s.name).startsWith(q)),
  ];
  // Sin filtro, la primera es la app que ya se ve: se propone la anterior.
  switcherIndex = !q && switcherItems[0]?.id === active && switcherItems.length > 1 ? 1 : 0;
  switcher.empty.hidden = switcherItems.length > 0;
  switcher.list.replaceChildren(
    ...switcherItems.map((s, i) => {
      const li = document.createElement("li");
      li.role = "option";
      const icon = document.createElement("span");
      icon.className = "m-icon";
      if (s.icon) {
        const img = document.createElement("img");
        img.src = s.icon;
        img.alt = "";
        icon.append(img);
      } else {
        icon.style.background = s.color;
        icon.textContent = s.name.trim()[0]?.toUpperCase() ?? "?";
      }
      const text = document.createElement("span");
      text.className = "m-text";
      const name = document.createElement("strong");
      name.textContent = s.name;
      text.append(name);
      if (s.group) {
        const group = document.createElement("small");
        group.textContent = s.group;
        text.append(group);
      }
      const badge = document.createElement("span");
      badge.className = "switcher-badge";
      const count = badges.get(s.id) ?? 0;
      badge.textContent = count > 0 ? String(count) : "";
      li.append(icon, text, badge);
      li.addEventListener("click", () => goToSwitcherItem(i));
      li.addEventListener("mousemove", () => {
        if (switcherIndex !== i) moveSwitcher(i - switcherIndex);
      });
      return li;
    }),
  );
  markSwitcher();
}

function markSwitcher() {
  for (const [i, li] of [...switcher.list.children].entries()) {
    li.setAttribute("aria-selected", String(i === switcherIndex));
    if (i === switcherIndex) li.scrollIntoView({ block: "nearest" });
  }
}

function moveSwitcher(step: number) {
  const n = switcherItems.length;
  if (n === 0) return;
  switcherIndex = (((switcherIndex + step) % n) + n) % n;
  markSwitcher();
}

function goToSwitcherItem(i: number) {
  const s = switcherItems[i];
  if (!s) return;
  returnTo = null;
  closeOverlay();
  select(s.id);
}

switcher.search.addEventListener("input", renderSwitcher);
switcher.search.addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    moveSwitcher(e.key === "ArrowDown" ? 1 : -1);
  } else if (e.key === "Enter") {
    e.preventDefault();
    goToSwitcherItem(switcherIndex);
  }
});

// ---------------------------------------------------------------- actualizaciones

const updateUi = {
  title: $("#update-title"),
  body: $("#update-body"),
  progress: $<HTMLProgressElement>("#update-progress"),
  error: $("#update-error"),
  later: $<HTMLButtonElement>("#update-later"),
  install: $<HTMLButtonElement>("#update-install"),
};

/// Pregunta por la versión nueva, sin tapar un diálogo que esté abierto.
function offerUpdate(version: string) {
  const view = overlay.hidden ? null : currentView();
  if (view !== null && !isBase(view)) return;
  updateUi.title.textContent = t("update.title", { version });
  updateUi.body.textContent = t("update.body");
  updateUi.progress.hidden = true;
  updateUi.error.textContent = "";
  updateUi.later.disabled = updateUi.install.disabled = false;
  openOverlay("update");
}

/// Hay una actualización descargándose.
let updating = false;

updateUi.install.addEventListener("click", async () => {
  updating = true;
  updateUi.later.disabled = updateUi.install.disabled = true;
  updateUi.error.textContent = "";
  updateUi.progress.hidden = false;
  updateUi.progress.removeAttribute("value");
  try {
    // Si sale bien, Pulpo se cierra y arranca la versión nueva.
    await invoke("install_update");
    updateUi.body.textContent = t("update.restarting");
  } catch (err) {
    updating = false;
    updateUi.progress.hidden = true;
    updateUi.error.textContent = tError(err);
    updateUi.later.disabled = updateUi.install.disabled = false;
  }
});

await listen<{ downloaded: number; total: number }>("update-progress", ({ payload }) => {
  const done = payload.total > 0 ? payload.downloaded / payload.total : 0;
  updateUi.progress.value = done;
  updateUi.body.textContent =
    done >= 1 ? t("update.restarting") : t("update.downloading", { percent: Math.floor(done * 100) });
});

// ---------------------------------------------------------------- arranque

// ---------------------------------------------------------------- idioma

const languageButtons = $("#language");

function renderLanguages() {
  languageButtons.replaceChildren(
    ...LANGUAGES.map((l) => {
      const b = document.createElement("button");
      b.type = "button";
      b.role = "radio";
      b.className = "ghost-btn";
      b.textContent = l.label;
      b.setAttribute("aria-checked", String(l.value === currentLanguage()));
      b.addEventListener("click", () => changeLanguage(l.value));
      return b;
    }),
  );
}

const themeButtons = $("#theme");

function renderThemes() {
  themeButtons.replaceChildren(
    ...THEMES.map((value) => {
      const b = document.createElement("button");
      b.type = "button";
      b.role = "radio";
      b.className = "ghost-btn";
      b.textContent = t(`manage.theme.${value}`);
      b.setAttribute("aria-checked", String(value === currentTheme()));
      b.addEventListener("click", () => {
        setTheme(value);
        renderThemes();
      });
      return b;
    }),
  );
}

/// Menú del icono de la bandeja (Windows) en el idioma de la interfaz.
const sendTrayTexts = () =>
  invoke("set_tray_texts", { show: t("tray.show"), quit: t("tray.quit") }).catch(console.error);

function changeLanguage(lang: Language) {
  setLanguage(lang);
  sendTrayTexts();
  renderLanguages();
  renderThemes();
  greeting = "";
  newGreeting();
  // Los textos que se pintan desde aquí se rehacen con el idioma nuevo.
  renderSidebar();
  renderManage();
  renderCi();
  renderCiTree();
  if (!ci.tokenBox.hidden) renderTokenRepos();
  renderSortings();
  const base = baseView();
  if (base) renderBase(base);
  showStatus(lastStatus);
  invoke<ZoomInfo>("get_zoom").then(showZoom).catch(console.error);
}

renderLanguages();
renderThemes();
applyI18n();
newGreeting();
tickClock();
sendTrayTexts();

homeApp = await invoke<string | null>("get_home_app");
applyPaneSize(await invoke<number>("get_home_pane_size"));
services = await invoke<Service[]>("list_services");
dndActive = new Set(await invoke<string[]>("dnd_active"));
active = await invoke<string | null>("active_service");
if (active) recent = [active];
renderSidebar();
showBase();
showStatus(await invoke<BlockerStatus>("blocker_status"));

await listen<BlockerStatus>("blocker-status", (e) => showStatus(e.payload));
await listen<string | null>("home-app-changed", (e) => {
  homeApp = e.payload;
  syncHomePane();
});
await listen<Service[]>("services-changed", (e) => {
  services = e.payload;
  if (homeApp && !services.some((s) => s.id === homeApp && s.enabled)) {
    document.body.classList.remove("split");
  } else {
    syncHomePane();
  }
  if (active !== null && !services.some((s) => s.id === active)) active = null;
  renderSidebar();
  markCurrent();
  if (currentView() === "manage" && !overlay.hidden) renderManage();
  else if (currentView() === "switcher" && !overlay.hidden) renderSwitcher();
  else showBase();
});
await listen<{ id: string; count: number }>("badge", ({ payload }) => {
  badges.set(payload.id, payload.count);
  // El contador de un grupo plegado suma los de sus apps.
  const group = services.find((s) => s.id === payload.id)?.group;
  if (group && collapsed.has(group)) return renderSidebar();
  const badge = nav.querySelector(`.service[data-id="${CSS.escape(payload.id)}"] .badge`);
  if (badge) badge.textContent = payload.count > 0 ? String(payload.count) : "";
  if (!overlay.hidden && currentView() === "home") renderHome();
});
await listen<string[]>("dnd-changed", ({ payload }) => {
  dndActive = new Set(payload);
  renderSidebar();
  if (!overlay.hidden && currentView() === "home") renderHome();
});
// Atajos pulsados dentro de una app (los detecta su script de inicio).
type Shortcut =
  | { kind: "home" }
  | { kind: "previous" }
  | { kind: "index"; n: number }
  | { kind: "switcher" };
await listen<Shortcut>("shortcut", ({ payload }) => {
  if (payload.kind === "switcher") return openSwitcher();
  returnTo = null;
  if (!overlay.hidden && !isBase(currentView())) closeOverlay();
  if (payload.kind === "home") goHome();
  else if (payload.kind === "previous") selectPrevious();
  else selectIndex(payload.n);
});
// Clic en una notificación de una app: Pulpo ya está al frente, falta abrirla.
/// Lleva a una app desde Rust: clic en una notificación suya o un enlace que
/// abre una regla en ella.
const showApp = ({ payload: id }: { payload: string }) => {
  if (!services.some((s) => s.id === id)) return;
  returnTo = null;
  if (!overlay.hidden && !isBase(currentView())) closeOverlay();
  select(id);
};
await listen<string>("notification-clicked", showApp);
await listen<string>("show-app", showApp);
await listen<string>("edit-service", ({ payload: id }) => {
  const s = services.find((s) => s.id === id);
  if (s) openForm(s, s.id);
});
await listen<string>("confirm-remove", ({ payload: id }) => askRemove(id));

/// Esc cierra el diálogo abierto; tres seguidos (en menos de un segundo)
/// llevan a la página inicial, igual que dentro de una app.
let escapes: number[] = [];
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !e.repeat) {
    const now = performance.now();
    escapes = [...escapes.filter((t) => now - t < 1000), now];
    if (escapes.length >= 3) {
      escapes = [];
      returnTo = null;
      if (!overlay.hidden && !isBase(currentView())) closeOverlay();
      return goHome();
    }
    if (!isBase(currentView())) closeOverlay();
    return;
  }
  // Ctrl+Espacio con el foco en la barra lateral (dentro de una app lo hace
  // su script de inicio).
  if (e.code === "Space" && e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey) {
    e.preventDefault();
    if (!e.repeat) openSwitcher();
    return;
  }
  // Ctrl+1..9 con el foco en la barra lateral (dentro de una app lo hace su
  // script de inicio).
  const digit = /^Digit([1-9])$/.exec(e.code);
  if (digit && e.ctrlKey && !e.altKey && !e.shiftKey) {
    if (!overlay.hidden && !isBase(currentView())) return;
    e.preventDefault();
    selectIndex(Number(digit[1]));
  }
});

// Ctrl Ctrl (dos toques de Ctrl solo) con el foco en la barra lateral.
let ctrlAlone = false;
let lastCtrlTap = -Infinity;
window.addEventListener("keydown", (e) => {
  ctrlAlone = e.key === "Control" ? !e.repeat || ctrlAlone : false;
});
window.addEventListener("keyup", (e) => {
  if (e.key !== "Control" || !ctrlAlone) return;
  ctrlAlone = false;
  const now = performance.now();
  if (now - lastCtrlTap < 400) {
    lastCtrlTap = -Infinity;
    if (overlay.hidden || isBase(currentView())) selectPrevious();
  } else {
    lastCtrlTap = now;
  }
});
window.addEventListener("blur", () => (ctrlAlone = false));

$("#home").addEventListener("click", goHome);

// ---------------------------------------------------------------- CI

/// Un run de GitHub Actions (como `ci::Run`).
interface CiRun {
  id: number;
  repo: string;
  workflow: string;
  branch: string | null;
  title: string;
  status: string;
  conclusion: string | null;
  url: string;
  started_at: string | null;
  updated_at: string | null;
}

interface CiAccount {
  /// Una cuenta por token (varios tokens pueden tener el mismo login).
  id: string;
  login: string;
  /// Dueños de los repos a los que llega el token.
  owners: string[];
  /// Nombre que le dio el usuario.
  name: string | null;
  valid: boolean;
}

interface CiRepo {
  repo: string;
  account: string;
  private: boolean;
  watched: boolean;
}

const ci = {
  button: $("#ci"),
  runs: $("#ci-runs"),
  empty: $("#ci-empty"),
  tree: $("#ci-tree"),
  error: $("#ci-error"),
  repoSearch: $<HTMLInputElement>("#ci-repo-search"),
  token: $<HTMLInputElement>("#ci-token"),
  tokenName: $<HTMLInputElement>("#ci-token-name"),
  tokenCheck: $<HTMLButtonElement>("#ci-token-check"),
  tokenBox: $("#ci-token-box"),
  tokenLogin: $("#ci-token-login"),
  tokenSearch: $<HTMLInputElement>("#ci-token-search"),
  tokenRepos: $("#ci-token-repos"),
  tokenError: $("#ci-token-error"),
  tokenOk: $<HTMLButtonElement>("#ci-token-ok"),
};
let ciRuns: CiRun[] = [];
let ciAccounts: CiAccount[] = [];
/// Repos a los que llegan los tokens (null: sin cargar todavía).
let ciRepos: CiRepo[] | null = null;
let ciReposLoading = false;

type CiState = "running" | "pending" | "ok" | "fail" | "off";

function ciState(r: CiRun): CiState {
  if (r.status === "in_progress") return "running";
  if (r.status !== "completed") return "pending";
  if (r.conclusion === "success") return "ok";
  if (["failure", "timed_out", "startup_failure", "action_required"].includes(r.conclusion ?? "")) {
    return "fail";
  }
  return "off";
}

function ciStatusText(r: CiRun): string {
  if (r.status === "in_progress" || r.status === "waiting") return t(`ci.state.${r.status}`);
  if (r.status !== "completed") return t("ci.state.queued");
  const state = ciState(r);
  if (state === "ok") return t("ci.state.success");
  if (state === "fail") return t("ci.state.failure");
  return r.conclusion === "cancelled" ? t("ci.state.cancelled") : t("ci.state.other");
}

/// Tiempo del run: hasta ahora si sigue, hasta su última actualización si acabó.
function ciDuration(r: CiRun): string {
  if (!r.started_at) return "";
  const start = Date.parse(r.started_at);
  const end = r.status === "completed" && r.updated_at ? Date.parse(r.updated_at) : Date.now();
  const secs = Math.max(0, Math.round((end - start) / 1000));
  if (secs < 60) return `${secs} s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} min ${String(secs % 60).padStart(2, "0")} s`;
  return `${Math.floor(mins / 60)} h ${mins % 60} min`;
}

const repoName = (repo: string) => repo.split("/").pop() ?? repo;

function renderCiButton() {
  const active = ciRuns.filter((r) => r.status !== "completed").length;
  ci.button.querySelector(".badge")!.textContent = active > 0 ? String(active) : "";
  const finished = ciRuns.filter((r) => r.status === "completed").map(ciState);
  ci.button.dataset.state = active
    ? "running"
    : finished.includes("fail")
      ? "fail"
      : finished.includes("ok")
        ? "ok"
        : "";
}

function renderCi() {
  renderCiButton();
  ci.runs.replaceChildren(
    ...ciRuns.map((r) => {
      const li = document.createElement("li");
      li.classList.toggle("finished", r.status === "completed");
      li.title = r.url;
      const dot = document.createElement("span");
      dot.className = "ci-dot";
      const state = ciState(r);
      dot.dataset.state = state;
      dot.textContent = state === "ok" ? "✓" : state === "fail" ? "✕" : "";
      const text = document.createElement("span");
      text.className = "m-text";
      const name = document.createElement("strong");
      name.textContent = `${repoName(r.repo)} · ${r.workflow}`;
      const detail = document.createElement("small");
      detail.textContent = [r.branch, r.title].filter(Boolean).join(" · ");
      text.append(name, detail);
      const time = document.createElement("span");
      time.className = "ci-time";
      time.dataset.id = String(r.id);
      time.append(ciStatusText(r));
      const elapsed = document.createElement("small");
      elapsed.textContent = ciDuration(r);
      time.append(elapsed);
      li.append(dot, text, time);
      // Terminados: se pueden quitar. Los fallidos no se van solos.
      if (r.status === "completed") {
        const dismiss = document.createElement("button");
        dismiss.className = "ci-dismiss";
        dismiss.textContent = "✕";
        dismiss.title = t("ci.remove");
        dismiss.addEventListener("click", (e) => {
          e.stopPropagation();
          invoke("ci_dismiss", { id: r.id }).catch(console.error);
        });
        li.append(dismiss);
      }
      li.addEventListener("click", () => invoke("ci_open", { url: r.url }).catch(console.error));
      return li;
    }),
  );
  const watched = ciRepos ? ciRepos.some((r) => r.watched) : ciAccounts.length > 0;
  ci.empty.textContent = ciRuns.length
    ? ""
    : !ciAccounts.length
      ? t("ci.noAccounts")
      : !watched
        ? t("ci.noRepos")
        : t("ci.empty");
}

/// Con la vista abierta, los tiempos de los runs en marcha avanzan solos.
setInterval(() => {
  if (overlay.hidden || currentView() !== "ci") return;
  for (const r of ciRuns) {
    if (r.status === "completed") continue;
    const small = ci.runs.querySelector(`.ci-time[data-id="${r.id}"] small`);
    if (small) small.textContent = ciDuration(r);
  }
}, 1000);

function repoLabel(r: CiRepo, onChange: (checked: boolean) => void): HTMLLabelElement {
  const label = document.createElement("label");
  label.className = "check";
  const box = document.createElement("input");
  box.type = "checkbox";
  box.checked = r.watched;
  box.addEventListener("change", () => onChange(box.checked));
  const name = document.createElement("span");
  name.textContent = r.repo;
  if (r.private) {
    const small = document.createElement("small");
    small.textContent = ` · ${t("ci.private")}`;
    name.append(small);
  }
  label.append(box, name);
  return label;
}

/// Configuración: cada token con sus repos debajo, para marcarlos o quitar el
/// token. Un repo se vigila con un solo token: marcarlo bajo otro lo mueve.
function renderCiTree() {
  const query = ci.repoSearch.value.trim().toLowerCase();
  if (!ciAccounts.length) {
    ci.tree.textContent = t("ci.noAccounts");
    return;
  }
  ci.tree.replaceChildren(
    ...ciAccounts.map((a) => {
      const group = document.createElement("div");
      group.className = "ci-tree-account";
      const head = document.createElement("div");
      head.className = "ci-account";
      const name = document.createElement("span");
      const title = document.createElement("strong");
      title.textContent = a.name ?? a.login;
      const detail = document.createElement("small");
      // Los dueños, si no es solo el propio usuario.
      const owners = a.owners.length === 1 && a.owners[0] === a.login ? "" : a.owners.join(", ");
      detail.textContent = [a.name ? a.login : null, owners]
        .filter(Boolean)
        .map((x) => ` · ${x}`)
        .join("");
      name.append(title, detail);
      if (!a.valid) {
        const warn = document.createElement("small");
        warn.className = "warn";
        warn.textContent = ` · ${t("ci.invalid")}`;
        name.append(warn);
      }
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "ghost-btn";
      remove.textContent = t("ci.remove");
      remove.addEventListener("click", () => {
        invoke("ci_remove_account", { id: a.id })
          .then(() => loadCiRepos())
          .catch((e) => (ci.error.textContent = tError(e)));
      });
      head.append(name, remove);
      const list = document.createElement("div");
      list.className = "ci-tree-repos";
      if (ciRepos === null) {
        list.textContent = ciReposLoading ? t("ci.reposLoading") : "";
      } else {
        const repos = ciRepos.filter(
          (r) => r.account === a.id && r.repo.toLowerCase().includes(query),
        );
        if (!repos.length) list.textContent = t("ci.reposEmpty");
        list.append(
          ...repos.map((r) =>
            repoLabel(r, (checked) => {
              for (const x of ciRepos ?? []) {
                if (x.repo === r.repo) x.watched = checked && x.account === a.id;
              }
              const watched = (ciRepos ?? [])
                .filter((x) => x.watched)
                .map(({ repo, account }) => ({ repo, account }));
              invoke("ci_set_repos", { repos: watched }).catch(console.error);
              renderCiTree();
              renderCi();
            }),
          ),
        );
      }
      group.append(head, list);
      return group;
    }),
  );
}

function loadCiRepos() {
  if (!ciAccounts.length) {
    ciRepos = [];
    renderCiTree();
    return;
  }
  ciRepos = null;
  ciReposLoading = true;
  renderCiTree();
  invoke<CiRepo[]>("ci_list_repos")
    .then((repos) => {
      ciRepos = repos;
      ci.error.textContent = "";
    })
    .catch((e) => (ci.error.textContent = tError(e)))
    .finally(() => {
      ciReposLoading = false;
      renderCiTree();
      renderCi();
    });
}

/// Configuración (tokens y repos), en un modal encima de la página de CI: al
/// cerrarse vuelve a ella.
function openCiSetup() {
  ci.error.textContent = "";
  loadCiRepos();
  returnTo = "ci";
  openOverlay("ciSetup");
}

// ------------------------------------------------ añadir token

/// Token comprobado (sus repos están en la lista) y sus repos.
let checkedToken: string | null = null;
let tokenRepos: CiRepo[] = [];

function renderTokenRepos() {
  const query = ci.tokenSearch.value.trim().toLowerCase();
  const repos = tokenRepos.filter((r) => r.repo.toLowerCase().includes(query));
  ci.tokenRepos.replaceChildren(
    ...repos.map((r) => repoLabel(r, (checked) => (r.watched = checked))),
  );
  if (!tokenRepos.length) ci.tokenRepos.textContent = t("ci.reposEmpty");
}

function openCiToken() {
  checkedToken = null;
  tokenRepos = [];
  ci.token.value = "";
  ci.tokenName.value = "";
  ci.tokenSearch.value = "";
  ci.tokenBox.hidden = true;
  ci.tokenError.textContent = "";
  ci.tokenOk.disabled = true;
  openOverlay("ciToken");
  ci.tokenName.focus();
}

function checkToken() {
  const token = ci.token.value.trim();
  ci.tokenError.textContent = "";
  ci.tokenCheck.disabled = true;
  ci.tokenCheck.textContent = t("ci.checking");
  invoke<{ login: string; repos: CiRepo[] }>("ci_check_token", { token })
    .then(({ login, repos }) => {
      checkedToken = token;
      tokenRepos = repos;
      ci.tokenLogin.textContent = t("ci.tokenRepos", { login });
      ci.tokenBox.hidden = false;
      ci.tokenOk.disabled = false;
      renderTokenRepos();
    })
    .catch((e) => (ci.tokenError.textContent = tError(e)))
    .finally(() => {
      ci.tokenCheck.disabled = false;
      ci.tokenCheck.textContent = t("ci.check");
    });
}

ci.button.addEventListener("click", () => {
  renderCi();
  openOverlay("ci");
  // Sin tokens no hay nada que ver: directo a la configuración.
  if (!ciAccounts.length) openCiSetup();
});
$("#ci-setup-open").addEventListener("click", openCiSetup);
$("#ci-token-open").addEventListener("click", openCiToken);
ci.repoSearch.addEventListener("input", renderCiTree);
ci.tokenSearch.addEventListener("input", renderTokenRepos);
ci.tokenCheck.addEventListener("click", checkToken);
// Otro token: hay que volver a comprobarlo.
ci.token.addEventListener("input", () => {
  if (checkedToken === null) return;
  checkedToken = null;
  ci.tokenBox.hidden = true;
  ci.tokenOk.disabled = true;
});
views.ciToken.addEventListener("submit", (e) => {
  e.preventDefault();
  // Enter sin comprobar: comprueba.
  if (checkedToken === null) return checkToken();
  ci.tokenOk.disabled = true;
  const repos = tokenRepos.filter((r) => r.watched).map((r) => r.repo);
  invoke<string>("ci_add_token", { token: checkedToken, name: ci.tokenName.value, repos })
    .then(() => openCiSetup())
    .catch((err) => {
      ci.tokenError.textContent = tError(err);
      ci.tokenOk.disabled = false;
    });
});
// Enlaces de la ayuda: al navegador del sistema (la barra lateral no navega).
overlay.addEventListener("click", (e) => {
  const link = (e.target as HTMLElement).closest<HTMLAnchorElement>("a[data-external]");
  if (!link) return;
  e.preventDefault();
  invoke("ci_open", { url: link.href }).catch(console.error);
});

const ciEnabledBox = $<HTMLInputElement>("#ci-enabled");
function showCiEnabled(enabled: boolean) {
  ci.button.hidden = !enabled;
  ciEnabledBox.checked = enabled;
}
ciEnabledBox.addEventListener("change", () =>
  invoke("ci_set_enabled", { enabled: ciEnabledBox.checked }).catch(console.error),
);
showCiEnabled(await invoke<boolean>("ci_enabled"));
await listen<boolean>("ci-enabled", ({ payload }) => showCiEnabled(payload));

ciAccounts = await invoke<CiAccount[]>("ci_accounts");
ciRuns = await invoke<CiRun[]>("ci_runs");
renderCi();
await listen<CiRun[]>("ci-changed", ({ payload }) => {
  ciRuns = payload;
  renderCi();
});
await listen<CiAccount[]>("ci-accounts", ({ payload }) => {
  ciAccounts = payload;
  renderCiTree();
  renderCi();
});
// Un run terminó: aviso del sistema (clic: el run en el navegador).
await listen<CiRun>("ci-finished", ({ payload: r }) => {
  const state = ciState(r);
  const mark = state === "ok" ? "✓" : state === "fail" ? "✕" : "•";
  invoke("ci_notify", {
    summary: `${mark} ${repoName(r.repo)} · ${r.workflow}`,
    body: [ciStatusText(r), r.branch, ciDuration(r)].filter(Boolean).join(" · "),
    url: r.url,
    state,
  }).catch(console.error);
});

// ---------------------------------------------------------------- fondo de pantalla

interface SavedWallpaper {
  name: string;
  data: string;
  current: boolean;
}

interface FoundWallpaper {
  id: string;
  thumb: string;
  full: string;
  resolution: string;
}

const wp = {
  error: $("#wallpaper-error"),
  saved: $("#wallpaper-saved"),
  results: $("#wallpaper-results"),
  query: $<HTMLInputElement>("#wallpaper-query"),
  sorting: $("#wallpaper-sorting"),
  more: $<HTMLButtonElement>("#wallpaper-more"),
  file: $<HTMLInputElement>("#wallpaper-file"),
};
const SORTINGS = ["toplist", "random", "date_added"] as const;
let wpSorting: (typeof SORTINGS)[number] = "toplist";
let wpPage = 0;
let wpLastPage = 1;
/// Cada búsqueda nueva invalida las respuestas de las anteriores.
let wpSearch = 0;

/// URL `blob:` del fondo activo: la imagen en base64 (varios MB) no cabe en
/// una variable CSS, Chromium la descarta.
let wallpaperUrl: string | null = null;

/// El fondo activo detrás de la página inicial.
async function applyWallpaper() {
  const data = await invoke<string | null>("wallpaper_get").catch(() => null);
  const blob = data ? await (await fetch(data)).blob().catch(() => null) : null;
  if (wallpaperUrl) URL.revokeObjectURL(wallpaperUrl);
  wallpaperUrl = blob ? URL.createObjectURL(blob) : null;
  document.body.classList.toggle("has-wallpaper", wallpaperUrl !== null);
  if (wallpaperUrl) document.body.style.setProperty("--wallpaper", `url("${wallpaperUrl}")`);
  else document.body.style.removeProperty("--wallpaper");
}

function wallpaperTile(src: string, label: string | null): HTMLDivElement {
  const tile = document.createElement("div");
  tile.className = "wallpaper-tile";
  tile.tabIndex = 0;
  tile.role = "button";
  const img = document.createElement("img");
  img.src = src;
  img.loading = "lazy";
  img.alt = "";
  tile.append(img);
  if (label) {
    const small = document.createElement("small");
    small.textContent = label;
    tile.append(small);
  }
  tile.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      tile.click();
    }
  });
  return tile;
}

const wpFail = (e: unknown) => (wp.error.textContent = tError(e));

/// Aviso breve abajo de la página del fondo ("Fondo aplicado").
let wpToastTimer: number | undefined;
function wpToast(key: "wallpaper.applied" | "wallpaper.removed") {
  const toast = $("#wallpaper-toast");
  toast.textContent = t(key);
  toast.classList.add("show");
  clearTimeout(wpToastTimer);
  wpToastTimer = window.setTimeout(() => toast.classList.remove("show"), 2000);
}
const wpApplied = () => {
  wp.error.textContent = "";
  wpToast("wallpaper.applied");
};

function loadSavedWallpapers() {
  invoke<SavedWallpaper[]>("wallpaper_list")
    .then((list) =>
      wp.saved.replaceChildren(
        ...list.map((w) => {
          const tile = wallpaperTile(w.data, null);
          tile.setAttribute("aria-current", String(w.current));
          tile.addEventListener("click", () =>
            invoke("wallpaper_use", { name: w.name }).then(wpApplied, wpFail),
          );
          const remove = document.createElement("button");
          remove.type = "button";
          remove.className = "remove";
          remove.textContent = "✕";
          remove.title = t("wallpaper.delete");
          remove.addEventListener("click", (e) => {
            e.stopPropagation();
            invoke("wallpaper_delete", { name: w.name }).catch(wpFail);
          });
          tile.append(remove);
          return tile;
        }),
      ),
    )
    .catch(wpFail);
}

function searchWallpapers(reset: boolean) {
  if (reset) {
    wpPage = 0;
    wp.results.replaceChildren();
  }
  const search = ++wpSearch;
  wp.more.hidden = true;
  invoke<{ items: FoundWallpaper[]; last_page: number }>("wallpaper_search", {
    query: wp.query.value,
    sorting: wpSorting,
    page: wpPage + 1,
  })
    .then(({ items, last_page }) => {
      if (search !== wpSearch) return;
      wpPage += 1;
      wpLastPage = last_page;
      wp.error.textContent = "";
      wp.results.append(
        ...items.map((w) => {
          const tile = wallpaperTile(w.thumb, w.resolution);
          tile.addEventListener("click", () => {
            if (tile.classList.contains("busy")) return;
            tile.classList.add("busy");
            invoke("wallpaper_download", { id: w.id, url: w.full })
              .then(wpApplied, wpFail)
              .finally(() => tile.classList.remove("busy"));
          });
          return tile;
        }),
      );
      wp.more.hidden = wpPage >= wpLastPage;
    })
    .catch((e) => search === wpSearch && wpFail(e));
}

function renderSortings() {
  wp.sorting.replaceChildren(
    ...SORTINGS.map((value) => {
      const b = document.createElement("button");
      b.type = "button";
      b.role = "radio";
      b.className = "ghost-btn";
      b.textContent = t(`wallpaper.sort.${value}`);
      b.setAttribute("aria-checked", String(value === wpSorting));
      b.addEventListener("click", () => {
        wpSorting = value;
        renderSortings();
        searchWallpapers(true);
      });
      return b;
    }),
  );
}

// Desde Gestionar apps; al cerrarse vuelve a él.
$("#wallpaper-open").addEventListener("click", () => {
  wp.error.textContent = "";
  renderSortings();
  loadSavedWallpapers();
  if (!wp.results.childElementCount) searchWallpapers(true);
  returnTo = "manage";
  openOverlay("wallpaper");
});
$("#wallpaper-own").addEventListener("click", () => wp.file.click());
$("#wallpaper-none").addEventListener("click", () =>
  invoke("wallpaper_use", { name: null })
    .then(() => wpToast("wallpaper.removed"))
    .catch(wpFail),
);
wp.more.addEventListener("click", () => searchWallpapers(false));
let wpQueryTimer: number | undefined;
wp.query.addEventListener("input", () => {
  clearTimeout(wpQueryTimer);
  wpQueryTimer = window.setTimeout(() => searchWallpapers(true), 500);
});
wp.file.addEventListener("change", () => {
  const file = wp.file.files?.[0];
  wp.file.value = "";
  if (!file) return;
  const reader = new FileReader();
  reader.onload = () => {
    const data = String(reader.result).split(",", 2)[1] ?? "";
    invoke("wallpaper_import", { name: file.name, data }).then(wpApplied, wpFail);
  };
  reader.readAsDataURL(file);
});
await listen("wallpaper-changed", () => {
  applyWallpaper();
  if (currentView() === "wallpaper") loadSavedWallpapers();
});
applyWallpaper();

// ---------------------------------------------------------------- tamaño

interface ZoomInfo {
  monitor: string | null;
  zoom: number;
  auto: boolean;
}

function showZoom(z: ZoomInfo) {
  $("#zoom-value").textContent = `${Math.round(z.zoom * 100)} %`;
  $("#zoom-monitor").textContent = z.monitor
    ? `(${z.monitor}${z.auto ? `, ${t("zoom.autoShort")}` : ""})`
    : "";
  $<HTMLButtonElement>("#zoom-auto").disabled = z.auto;
}

const changeZoom = (step: number) => invoke("change_zoom", { step }).catch(console.error);
$("#zoom-in").addEventListener("click", () => changeZoom(1));
$("#zoom-out").addEventListener("click", () => changeZoom(-1));
$("#zoom-auto").addEventListener("click", () => changeZoom(0));
window.addEventListener("keydown", (e) => {
  if (!e.ctrlKey || e.altKey) return;
  const step = e.key === "+" || e.key === "=" ? 1 : e.key === "-" ? -1 : e.key === "0" ? 0 : null;
  if (step === null) return;
  e.preventDefault();
  changeZoom(step);
});
showZoom(await invoke<ZoomInfo>("get_zoom"));
await listen<ZoomInfo>("zoom-changed", (e) => showZoom(e.payload));

// ---------------------------------------------------------------- Bitwarden

/// Como `bitwarden::Status`.
interface VaultStatus {
  state: "missing" | "unauthenticated" | "locked" | "unlocked";
  email: string | null;
}

interface VaultEntry {
  id: string;
  name: string;
  username: string;
}

const vault = {
  button: $<HTMLButtonElement>("#vault"),
  message: $("#vault-message"),
  password: $<HTMLInputElement>("#vault-password"),
  list: $("#vault-list"),
  error: $("#vault-error"),
  lock: $<HTMLButtonElement>("#vault-lock"),
  unlock: $<HTMLButtonElement>("#vault-unlock"),
};
/// App en la que se rellena.
let fillTarget: string | null = null;

function serviceName(id: string) {
  return services.find((s) => s.id === id)?.name ?? id;
}

/// Muestra el diálogo en uno de sus modos: un aviso, pedir la contraseña
/// maestra o elegir entre varias entradas.
/// `html` solo para textos fijos de i18n: los nombres de las apps van como texto.
function showVault(mode: "message" | "unlock" | "list", message: string, html = false) {
  if (html) vault.message.innerHTML = message;
  else vault.message.textContent = message;
  vault.password.hidden = vault.unlock.hidden = mode !== "unlock";
  vault.list.hidden = mode !== "list";
  // Bloquear solo con la bóveda abierta (la lista, o "nada para esta página").
  vault.lock.hidden = mode !== "list";
  vault.error.textContent = "";
  if (mode === "unlock") vault.password.value = "";
  if (currentView() !== "vault" || overlay.hidden) {
    returnTo = null;
    openOverlay("vault");
  }
  (mode === "unlock" ? vault.password : (vault.list.querySelector("button") as HTMLElement | null))?.focus();
}

/// Vuelve a la app con el foco en ella.
function backToApp() {
  if (currentView() === "vault" && !overlay.hidden) closeOverlay();
  if (fillTarget && fillTarget === active) {
    invoke("activate_service", { id: fillTarget }).catch(console.error);
  }
}

async function startFill(id: string) {
  fillTarget = id;
  try {
    const status = await invoke<VaultStatus>("bw_status");
    if (status.state === "missing") return showVault("message", t("vault.missing"));
    if (status.state === "unauthenticated") return showVault("message", t("vault.login"), true);
    if (status.state === "locked") {
      return showVault("unlock", t("vault.locked", { email: status.email ?? "" }));
    }
    await listLogins();
  } catch (e) {
    showVault("message", "");
    vault.error.textContent = tError(e);
  }
}

async function listLogins() {
  if (!fillTarget) return;
  const entries = await invoke<VaultEntry[]>("bw_logins", { service: fillTarget });
  // Una sola: se rellena sin preguntar.
  if (entries.length === 1) return fill(entries[0].id);
  if (!entries.length) {
    showVault("message", t("vault.none", { app: serviceName(fillTarget) }));
    vault.lock.hidden = false;
    return;
  }
  vault.list.replaceChildren(
    ...entries.map((entry) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "vault-entry";
      const name = document.createElement("strong");
      name.textContent = entry.name;
      const user = document.createElement("span");
      user.textContent = entry.username;
      b.append(name, user);
      b.addEventListener("click", () => fill(entry.id));
      return b;
    }),
  );
  showVault("list", t("vault.choose", { app: serviceName(fillTarget) }));
}

async function fill(id: string) {
  if (!fillTarget) return;
  try {
    await invoke("bw_fill", { service: fillTarget, id });
    backToApp();
  } catch (e) {
    showVault("message", "");
    vault.error.textContent = tError(e);
  }
}

views.vault.addEventListener("submit", (e) => {
  e.preventDefault();
  if (vault.unlock.hidden) return;
  vault.unlock.disabled = true;
  vault.error.textContent = "";
  invoke("bw_unlock", { password: vault.password.value })
    .then(() => listLogins())
    .catch((e) => {
      vault.error.textContent = tError(e);
      vault.password.select();
    })
    .finally(() => (vault.unlock.disabled = false));
});
vault.lock.addEventListener("click", () => {
  invoke("bw_lock").catch(console.error);
  closeOverlay();
});
vault.button.addEventListener("click", () => {
  if (active) startFill(active);
  else showVault("message", t("vault.noApp"));
});
// Ctrl+Mayús+L dentro de una app (Rust solo lo manda con Bitwarden activado).
await listen<string>("fill-request", ({ payload }) => startFill(payload));

const vaultEnabledBox = $<HTMLInputElement>("#vault-enabled");
function showVaultEnabled(enabled: boolean) {
  vault.button.hidden = !enabled;
  vaultEnabledBox.checked = enabled;
}
vaultEnabledBox.addEventListener("change", () =>
  invoke("bw_set_enabled", { enabled: vaultEnabledBox.checked }).catch(console.error),
);
showVaultEnabled(await invoke<boolean>("bw_enabled"));
await listen<boolean>("bw-enabled", ({ payload }) => showVaultEnabled(payload));

await setupMusic(select);

// Al final del arranque, sin esperar: GitHub puede tardar o no responder.
invoke<{ version: string } | null>("check_update")
  .then((found) => found && offerUpdate(found.version))
  .catch(console.error);
