// Names that differ per operating system.

const ua = navigator.userAgent;

export class Platform {
  static isMac = /Macintosh|Mac OS X/.test(ua);
  static isWindows = /Windows/.test(ua);

  /** Adds `mac` / `windows` / `linux` to <body> so styles can adapt. */
  static markBody() {
    document.body.classList.add(Platform.isMac ? "mac" : Platform.isWindows ? "windows" : "linux");
  }

  /** "Finder", "Explorer" or "file manager". */
  static fileManager() {
    return Platform.isMac ? "Finder" : Platform.isWindows ? "Explorer" : "file manager";
  }

  /** "menu bar" on a Mac, "system tray" elsewhere. */
  static tray() {
    return Platform.isMac ? "menu bar" : "system tray";
  }

  /** The modifier key symbol for shortcuts. */
  static modKey() {
    return Platform.isMac ? "⌘" : "Ctrl+";
  }
}
