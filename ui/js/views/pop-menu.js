// A small menu that opens under a button: "⋯" actions and "Open folder" choices.

import { Dom } from "../dom.js";
import { Icons } from "../icons.js";

export class PopMenu {
  constructor(element) {
    this.element = element;
    this.anchor = null;   // the button it opened under
    this.actions = [];
    element.addEventListener("click", event => {
      const button = event.target.closest("button[data-index]");
      if (!button || button.disabled) return;
      const action = this.actions[Number(button.dataset.index)];
      this.close();
      action?.();
    });
    document.addEventListener("pointerdown", event => {
      if (this.isOpen && !this.element.contains(event.target) && !event.target.closest("[data-menu-anchor]")) this.close();
    });
    window.addEventListener("blur", () => this.close());
  }

  get isOpen() {
    return this.element.classList.contains("open");
  }

  /** Opens under `anchor`. Items: `{ label, icon, danger, disabled, title, run }` or `"-"` for a line. */
  show(anchor, items) {
    this.anchor = anchor;
    this.actions = [];
    this.element.innerHTML = items
      .map(item => {
        if (item === "-") return "<hr>";
        const index = this.actions.push(item.run) - 1;
        return Dom.html`<button data-index="${index}" class="${item.danger ? "danger" : ""}" role="menuitem"
          ${item.disabled ? "disabled" : ""} title="${item.title ?? ""}">
          <span class="ico">${Dom.raw(Icons[item.icon] ?? "")}</span><span>${item.label}</span></button>`;
      })
      .join("");
    const box = anchor.getBoundingClientRect();
    this.element.style.top = `${box.bottom + 6}px`;
    this.element.style.right = `${Math.max(8, window.innerWidth - box.right)}px`;
    this.element.classList.add("open");
    this.element.setAttribute("aria-hidden", "false");
    // Keep the menu inside the window
    const menu = this.element.getBoundingClientRect();
    if (menu.bottom > window.innerHeight - 8) this.element.style.top = `${box.top - menu.height - 6}px`;
  }

  /** Closes; a focused item gives the focus back to the menu's button, so it isn't left on a hidden item. */
  close() {
    const focused = this.element.contains(document.activeElement);
    this.element.classList.remove("open");
    this.element.setAttribute("aria-hidden", "true");
    if (focused) this.anchor?.focus({ preventScroll: true });
  }
}
