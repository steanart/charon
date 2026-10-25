// Everything that opens over the window (sheets, the review dialog and modal questions)
// shares one stack. Esc and a click on the dimmed window close only the topmost overlay,
// and the topmost overlay owns the keyboard focus: everything under it is inert.

import { Dom } from "../dom.js";

const FOCUSABLE = [
  "button:not([disabled])",
  "[href]",
  'input:not([disabled]):not([type="hidden"])',
  "select:not([disabled])",
  "textarea:not([disabled])",
  "summary",
  '[tabindex]:not([tabindex="-1"])',
].join(", ");

const BASE_LAYER = 30;   // z-index of the lowest overlay; each one above it adds 2

/**
 * Where a control is, so it can be found again when its job row or list item was redrawn
 * while an overlay covered it: its own selector, and the row or item (`[data-id]`) around it.
 */
function addressOf(element) {
  if (!element || element === document.body) return null;
  const holder = element.parentElement?.closest("[data-id]");
  return {
    own: Dom.selectorOf(element),
    holder: holder ? `${holder.localName}[data-id="${CSS.escape(holder.dataset.id)}"]` : null,
  };
}

/**
 * The control at `address`, or else the first one left in its row or item. Without a row
 * or item, it's looked for in the overlay now on top (or the window when none is left).
 */
function findAgain(address) {
  if (!address) return null;
  const scope = address.holder ? document.querySelector(address.holder) : Overlay.top()?.element ?? document;
  if (!scope) return null;
  const own = address.own ? scope.querySelector(`${address.own}:not([disabled])`) : null;
  return own ?? (address.holder ? scope.querySelector(FOCUSABLE) : null);
}

export class Overlay {
  static stack = [];
  static closeListeners = [];

  constructor(element, { modal = false } = {}) {
    this.element = element;
    this.modal = modal;          // a question that must be answered before anything else
    this.returnFocus = null;     // where the focus goes back to when this closes
    this.returnAddress = null;   // how to find that control again if it was redrawn meanwhile
    element.inert = true;
    if (!element.hasAttribute("tabindex")) element.tabIndex = -1;
  }

  get isOpen() {
    return Overlay.stack.includes(this);
  }

  /** Shows the overlay on top of the others and moves the focus into it. */
  open() {
    if (this.isOpen) {
      Overlay.stack.splice(Overlay.stack.indexOf(this), 1);
    } else {
      this.returnFocus = document.activeElement;
      this.returnAddress = addressOf(document.activeElement);
    }
    Overlay.stack.push(this);
    this.element.classList.add("open");
    this.element.setAttribute("aria-hidden", "false");
    Overlay.arrange();
    this.focus();
  }

  close() {
    const index = Overlay.stack.indexOf(this);
    if (index < 0) return;
    Overlay.stack.splice(index, 1);
    this.element.classList.remove("open");
    this.element.setAttribute("aria-hidden", "true");
    this.element.inert = true;
    Overlay.arrange();
    // Back to the control that opened it, or the same control drawn anew, else the overlay underneath.
    const back = this.returnFocus?.isConnected ? this.returnFocus : findAgain(this.returnAddress);
    this.returnFocus = this.returnAddress = null;
    if (back && !back.closest("[inert]")) back.focus({ preventScroll: true });
    else Overlay.top()?.focus();
    for (const listener of Overlay.closeListeners) listener(this);
  }

  /** What Esc and a click on the dimmed window do: usually the same as closing. */
  dismiss() {
    this.close();
  }

  /** Moves the focus into the overlay, unless it's there already: to `[data-autofocus]`, else the overlay itself. */
  focus() {
    const active = document.activeElement;
    if (active !== this.element && this.element.contains(active)) return;
    const target = this.element.querySelector("[data-autofocus]:not([disabled])") ?? this.element;
    target.focus({ preventScroll: true });
  }

  /**
   * Redraws with `draw()` and keeps what the person had: the scroll position and the
   * focused control, when it's still there afterwards.
   */
  redraw(draw) {
    const active = document.activeElement;
    const key = active !== this.element && this.element.contains(active) ? Dom.selectorOf(active) : null;
    const scrolled = [...this.element.querySelectorAll(".scroll, .review-body")].map(el => el.scrollTop);
    draw();
    this.element.querySelectorAll(".scroll, .review-body").forEach((el, i) => (el.scrollTop = scrolled[i] ?? 0));
    const again = key && this.element.querySelector(key);
    if (again && !again.disabled) again.focus({ preventScroll: true });
    else if (Overlay.top() === this && !this.element.contains(document.activeElement)) this.focus();
  }

  /** The overlay on top, or null. */
  static top() {
    return Overlay.stack.at(-1) ?? null;
  }

  /** Whether a modal question is open. */
  static anyModal() {
    return Overlay.stack.some(overlay => overlay.modal);
  }

  /** Calls `listener(overlay)` each time an overlay has closed. */
  static afterClose(listener) {
    Overlay.closeListeners.push(listener);
  }

  /** Closes the topmost overlay the way Esc does. False when nothing was open. */
  static dismissTop() {
    const top = Overlay.top();
    top?.dismiss();
    return Boolean(top);
  }

  /** Layers the open overlays, puts the scrim right under the topmost one, and makes the rest inert. */
  static arrange() {
    const stack = Overlay.stack;
    stack.forEach((overlay, i) => {
      overlay.element.style.zIndex = BASE_LAYER + i * 2;
      overlay.element.inert = i !== stack.length - 1;
    });
    const scrim = document.querySelector("#scrim");
    scrim.classList.toggle("on", stack.length > 0);
    if (stack.length) scrim.style.zIndex = BASE_LAYER + (stack.length - 1) * 2 - 1;
    for (const part of document.querySelectorAll("body > header, body > section, body > main")) part.inert = stack.length > 0;
  }

  /** Keeps Tab inside the topmost overlay, also where `inert` isn't known. */
  static trapTab(event) {
    const top = Overlay.top();
    if (!top) return;
    const items = [...top.element.querySelectorAll(FOCUSABLE)].filter(el => el.getClientRects().length && !el.closest("[inert]"));
    const active = document.activeElement;
    if (!items.length) {
      event.preventDefault();
      top.element.focus();
    } else if (!top.element.contains(active)) {
      event.preventDefault();
      items[0].focus();
    } else if (event.shiftKey && (active === items[0] || active === top.element)) {
      event.preventDefault();
      items.at(-1).focus();
    } else if (!event.shiftKey && active === items.at(-1)) {
      event.preventDefault();
      items[0].focus();
    }
  }
}
