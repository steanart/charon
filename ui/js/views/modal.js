// A question that must be answered before anything else: warnings and confirmations.
// Cancel has the focus first, Esc and a click outside mean Cancel, and Enter never
// presses a red button (nor does ⌘/Ctrl+Enter anything at all).

import { Dom } from "../dom.js";
import { Icons } from "../icons.js";
import { Overlay } from "./overlay.js";

const { html, raw } = Dom;

const BUTTON_CLASSES = { danger: "btn danger-solid", caution: "btn caution", primary: "btn primary" };
let lastId = 0;

export class Modal extends Overlay {
  /**
   * Asks and waits for the answer: the `value` of the button pressed, or null for Cancel,
   * Esc or a click outside. `body` is a sentence or a list of paragraphs, `tone` "danger"
   * or "caution", and `tick` the label of a checkbox that buttons with `needsTick` wait for.
   * Buttons: `{ label, value, kind: "danger" | "caution" | "primary", focus, needsTick }`.
   */
  static ask(options) {
    return new Modal(options).answer;
  }

  /** Asks to go ahead with one thing; true only when its button is pressed. Cancel has the focus. */
  static async confirm({ title, body, action, tone = "danger" }) {
    const answer = await Modal.ask({
      title,
      body,
      tone,
      buttons: [
        { label: "Cancel", value: null, focus: true },
        { label: action, value: "yes", kind: tone === "danger" ? "danger" : "primary" },
      ],
    });
    return answer === "yes";
  }

  constructor({ title, body = [], tone = null, tick = null, buttons }) {
    const element = document.createElement("div");
    element.className = `modal ${tone ?? ""}`;
    element.setAttribute("role", "alertdialog");
    element.setAttribute("aria-modal", "true");
    document.body.append(element);
    super(element, { modal: true });
    this.buttons = buttons;
    this.answer = new Promise(resolve => (this.resolve = resolve));
    this.render({ title, body: [body].flat(), tone, tick });
    element.addEventListener("click", event => {
      const button = event.target.closest("button[data-index]");
      if (button && !button.disabled) this.finish(this.buttons[Number(button.dataset.index)].value ?? null);
    });
    element.addEventListener("change", () => this.updateButtons());
    element.addEventListener("keydown", event => this.onKey(event));
    element.getBoundingClientRect(); // lay it out first, so it fades in
    this.open();
  }

  render({ title, body, tone, tick }) {
    const id = `modal-${++lastId}`;
    this.element.setAttribute("aria-labelledby", `${id}-title`);
    this.element.setAttribute("aria-describedby", `${id}-body`);
    this.element.innerHTML = html`
      <h2 id="${id}-title">${raw(tone ? html`<span class="ico">${raw(Icons.warn)}</span>` : "")}<span>${title}</span></h2>
      <div class="body" id="${id}-body">${body.map(text => raw(html`<p>${text}</p>`))}</div>
      ${raw(tick ? html`<label class="tick"><input type="checkbox" data-tick><span>${tick}</span></label>` : "")}
      <footer>${this.buttons.map((b, i) => raw(html`
        <button class="${BUTTON_CLASSES[b.kind] ?? "btn ghost"}" data-index="${i}" data-kind="${b.kind ?? "plain"}"
          ${b.focus ? "data-autofocus" : ""} ${b.needsTick ? "disabled" : ""}>${b.label}</button>`))}</footer>`;
  }

  /** Buttons that wait for the tick follow it. */
  updateButtons() {
    const ticked = Boolean(Dom.$("[data-tick]", this.element)?.checked);
    this.buttons.forEach((b, i) => {
      if (b.needsTick) Dom.$(`[data-index="${i}"]`, this.element).disabled = !ticked;
    });
  }

  /** Enter presses only a plain button that has the focus; never a red one. */
  onKey(event) {
    if (event.key !== "Enter") return;
    event.stopPropagation();
    const button = event.target.closest?.("button[data-index]");
    if (event.metaKey || event.ctrlKey || !button || button.dataset.kind === "danger") event.preventDefault();
  }

  dismiss() {
    this.finish(null);
  }

  /** Answers, closes, and takes the element away once it has faded out. */
  finish(value) {
    if (!this.isOpen) return;
    this.close();
    this.resolve(value);
    setTimeout(() => this.element.remove(), 400);
  }
}
