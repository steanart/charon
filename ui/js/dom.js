// Small helpers for building markup safely.

export class Dom {
  /** Escapes text for use inside HTML. */
  static escape(value) {
    return String(value ?? "")
      .replaceAll("&", "&amp;")
      .replaceAll("<", "&lt;")
      .replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;")
      .replaceAll("'", "&#39;");
  }

  /** A tagged template that escapes every interpolated value, except `Dom.raw(...)` ones. */
  static html(strings, ...values) {
    return strings.reduce((out, string, i) => {
      if (i === 0) return string;
      const value = values[i - 1];
      const text = Array.isArray(value)
        ? value.map(v => (v instanceof Raw ? v.html : Dom.escape(v))).join("")
        : value instanceof Raw ? value.html : Dom.escape(value);
      return out + text + string;
    }, "");
  }

  /** Marks already-safe HTML so `Dom.html` doesn't escape it. Nothing (undefined or null) is empty. */
  static raw(html) {
    return new Raw(html ?? "");
  }

  /** A selector that finds the same control again after a redraw, from its data attributes. */
  static selectorOf(element) {
    const keys = ["do", "action", "id", "list", "index", "key", "choice", "field", "value", "tab", "trigger"];
    const parts = keys
      .filter(key => element.dataset?.[key] !== undefined)
      .map(key => `[data-${key}="${CSS.escape(element.dataset[key])}"]`);
    return parts.length ? element.localName + parts.join("") : null;
  }

  static $(selector, root = document) {
    return root.querySelector(selector);
  }

  static $$(selector, root = document) {
    return [...root.querySelectorAll(selector)];
  }
}

class Raw {
  constructor(html) { this.html = html; }
}
