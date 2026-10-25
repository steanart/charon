// A panel that slides in from the right, over a dimmed window.

import { Overlay } from "./overlay.js";

export class Sheet extends Overlay {
  static all = [];

  constructor(element) {
    super(element);
    Sheet.all.push(this);
  }

  /**
   * Opens the sheet. Other sheets close, unless `over` keeps them open underneath. A sheet
   * never covers a question or the review: while one is open, the sheet goes in under it
   * and the sheets already open stay as they are (the editor under a warning keeps its draft).
   */
  open({ over = false } = {}) {
    const above = Overlay.stack.filter(overlay => !(overlay instanceof Sheet));
    if (!over && !above.length) for (const other of Sheet.all) if (other !== this && other.isOpen) other.close();
    super.open();
    for (const overlay of above) overlay.open(); // back on top, in the same order
  }

  static closeAll() {
    for (const sheet of Sheet.all) sheet.close();
  }

  static anyOpen() {
    return Sheet.all.some(sheet => sheet.isOpen);
  }
}
