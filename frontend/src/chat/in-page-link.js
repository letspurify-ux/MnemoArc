// The workspace uses location.hash for the selected session. Markdown footnotes
// and SVG anchors should scroll within the document without replacing that ID.
export function followInPageLink(event) {
  const href = event.currentTarget.getAttribute("href");
  if (!href?.startsWith("#") || event.defaultPrevented ||
      event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  event.preventDefault();
  let id;
  try { id = decodeURIComponent(href.slice(1)); } catch { return; }
  const target = document.getElementById(id);
  if (!target) return;
  target.scrollIntoView({ block: "nearest" });
  if (target.tabIndex < 0 && !target.hasAttribute("tabindex")) {
    target.setAttribute("tabindex", "-1");
    target.addEventListener("blur", () => target.removeAttribute("tabindex"), { once: true });
  }
  target.focus({ preventScroll: true });
}
