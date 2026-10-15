import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { applyI18n, currentLanguage, type Language, LANGUAGES, setLanguage, t, tError } from "./i18n";

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
};
const form = {
  title: $("#form-title"),
  name: $<HTMLInputElement>("#f-name"),
  url: $<HTMLInputElement>("#f-url"),
  adblock: $<HTMLInputElement>("#f-adblock"),
  group: $<HTMLInputElement>("#f-group"),
  groups: $<HTMLDataListElement>("#groups"),
  iconPreview: $("#f-icon"),
  iconFile: $<HTMLInputElement>("#f-icon-file"),
  error: $("#f-error"),
  submit: $<HTMLButtonElement>("#f-submit"),
};

let services: Service[] = [];
let active: string | null = null;
const badges = new Map<string, number>();

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
  showBase();
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
  markCurrent();
  invoke("show_home").catch(console.error);
  showBase();
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
/// se vuelve a ella).
function showBase() {
  const current = overlay.hidden ? null : currentView();
  if (current !== null && !isBase(current)) return;
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
  button.title = [s.name, index < 9 ? `Ctrl+${index + 1}` : "", s.enabled ? "" : t("service.disabled")]
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

function openOverlay(view: View) {
  for (const [name, el] of Object.entries(views)) el.hidden = name !== view;
  overlay.classList.toggle("menu-mode", view === "menu");
  overlay.classList.toggle("home-mode", view === "home");
  if (!overlay.hidden) return;
  invoke("set_overlay", { open: true })
    .catch(console.error)
    .finally(() => {
      overlay.hidden = false;
      document.body.classList.add("overlay-open");
      if (!isBase(view)) {
        (views[view].querySelector("input, button:not(.ghost)") as HTMLElement | null)?.focus();
      }
    });
}

function hideOverlay() {
  overlay.hidden = true;
  document.body.classList.remove("overlay-open");
  invoke("set_overlay", { open: false }).catch(console.error);
}

/// Cierra el diálogo abierto: vuelve a la vista de la que vino, a la vista
/// base, o deja ver la app.
function closeOverlay() {
  if (overlay.hidden) return;
  const current = currentView();
  editing = removing = null;
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
  for (const [name, el] of Object.entries(views)) {
    if (!el.hidden) return name as View;
  }
  return null;
}

function renderManage() {
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
        }).catch(
          console.error,
        ),
      { checked: s.adblock },
    ),
    sep(),
    item(t("action.editMenu"), () => openForm(s, s.id), { dialog: true }),
    item(t(s.enabled ? "action.disable" : "action.enable"), () =>
      invoke("set_service_enabled", { id: s.id, enabled: !s.enabled }).catch(console.error),
    ),
    item(t("action.removeMenu"), () => askRemove(s.id), { danger: true, dialog: true }),
  );
  menu.style.top = `${Math.max(8, Math.min(top, window.innerHeight - 260))}px`;
  menu.style.left = left === undefined ? "" : `${Math.min(left, window.innerWidth - 230)}px`;
  openOverlay("menu");
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
});

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
}

function openForm(init: FormInit, edit: string | null) {
  editing = edit;
  form.title.textContent = t(edit ? "form.edit" : "form.new");
  form.submit.textContent = t(edit ? "action.save" : "action.add");
  form.name.value = init.name;
  form.url.value = init.url;
  form.adblock.checked = init.adblock;
  form.group.value = init.group ?? "";
  form.groups.replaceChildren(
    ...[...new Set(services.map((s) => s.group).filter(Boolean))].map((g) =>
      Object.assign(document.createElement("option"), { value: g! }),
    ),
  );
  formColor = init.color ?? "#8e4ec6";
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

// ---------------------------------------------------------------- arranque

// ---------------------------------------------------------------- idioma

const languageSelect = $<HTMLSelectElement>("#language");
languageSelect.replaceChildren(
  ...LANGUAGES.map((l) => Object.assign(document.createElement("option"), { value: l.value, textContent: l.label })),
);
languageSelect.value = currentLanguage();
languageSelect.addEventListener("change", () => {
  setLanguage(languageSelect.value as Language);
  // Los textos que se pintan desde aquí se rehacen con el idioma nuevo.
  renderSidebar();
  renderManage();
  const base = baseView();
  if (base) renderBase(base);
  showStatus(lastStatus);
  invoke<ZoomInfo>("get_zoom").then(showZoom).catch(console.error);
});
applyI18n();

services = await invoke<Service[]>("list_services");
active = await invoke<string | null>("active_service");
if (active) recent = [active];
renderSidebar();
showBase();
showStatus(await invoke<BlockerStatus>("blocker_status"));

await listen<BlockerStatus>("blocker-status", (e) => showStatus(e.payload));
await listen<Service[]>("services-changed", (e) => {
  services = e.payload;
  if (active !== null && !services.some((s) => s.id === active)) active = null;
  renderSidebar();
  markCurrent();
  if (currentView() === "manage" && !overlay.hidden) renderManage();
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
// Atajos pulsados dentro de una app (los detecta su script de inicio).
type Shortcut = { kind: "home" } | { kind: "previous" } | { kind: "index"; n: number };
await listen<Shortcut>("shortcut", ({ payload }) => {
  returnTo = null;
  if (!overlay.hidden && !isBase(currentView())) closeOverlay();
  if (payload.kind === "home") goHome();
  else if (payload.kind === "previous") selectPrevious();
  else selectIndex(payload.n);
});
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
