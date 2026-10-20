// Ctrl/Shift+clic y clic central en un enlace -> window.open (ver `links::SCRIPT`).
(() => {
  if (window.__pulpoLinks) return;
  window.__pulpoLinks = true;

  const onClick = (e) => {
    const modified = e.type === "click" && e.button === 0 && (e.ctrlKey || e.metaKey || e.shiftKey);
    const middle = e.type === "auxclick" && e.button === 1;
    if (!modified && !middle) return;
    const a = e.composedPath().find((n) => n instanceof HTMLAnchorElement || n instanceof HTMLAreaElement);
    if (!a || !a.href || a.hasAttribute("download")) return;
    let url;
    try {
      url = new URL(a.href, location.href);
    } catch {
      return;
    }
    if (url.protocol !== "http:" && url.protocol !== "https:") return;
    // La acción nativa gasta la activación del usuario y window.open quedaría
    // bloqueado: se cancela ya. La página no se entera: ve solo su propio
    // preventDefault, y si lo llama (porque atiende el clic ella misma) no se
    // hace nada.
    e.preventDefault();
    let prevented = false;
    Object.defineProperty(e, "preventDefault", { value: () => (prevented = true) });
    Object.defineProperty(e, "defaultPrevented", { get: () => prevented });
    Object.defineProperty(e, "returnValue", {
      get: () => !prevented,
      set: (v) => (prevented ||= !v),
    });
    setTimeout(() => {
      if (!prevented) window.open(url.href, "_blank", "noopener");
    });
  };
  window.addEventListener("click", onClick, true);
  window.addEventListener("auxclick", onClick, true);
})();
