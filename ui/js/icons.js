// Every icon the window uses, as inline SVG drawn with currentColor.

const stroke = d => `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">${d}</svg>`;

export const Icons = {
  sync: stroke('<path d="M13.5 8a5.5 5.5 0 0 1-9.6 3.7M2.5 8a5.5 5.5 0 0 1 9.6-3.7"/><path d="M12.4 1.8v2.6H9.8M3.6 14.2v-2.6h2.6"/>'),
  play: stroke('<path d="M5 3.2v9.6L12.6 8z"/>'),
  stop: stroke('<rect x="4" y="4" width="8" height="8" rx="1.5"/>'),
  pause: stroke('<path d="M5.5 3.5v9M10.5 3.5v9"/>'),
  resume: stroke('<path d="M5 3.2v9.6L12.6 8z"/>'),
  plus: stroke('<path d="M8 3v10M3 8h10"/>'),
  settings: `<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.35" stroke-linejoin="round"><path d="M8.21 3.85 8.52 1.53h2.96l.31 2.32 1.29.54 1.87-1.43 2.09 2.09-1.43 1.87.54 1.29 2.32.31v2.96l-2.32.31-.54 1.29 1.43 1.87-2.09 2.09-1.87-1.43-1.29.54-.31 2.32H8.52l-.31-2.32-1.29-.54-1.87 1.43-2.09-2.09 1.43-1.87-.54-1.29-2.32-.31V8.52l2.32-.31.54-1.29-1.43-1.87 2.09-2.09 1.87 1.43z"/><circle cx="10" cy="10" r="2.7"/></svg>`,
  info: stroke('<circle cx="8" cy="8" r="6.2"/><path d="M8 7.2v4"/><circle cx="8" cy="4.9" r=".5" fill="currentColor"/>'),
  search: stroke('<circle cx="7" cy="7" r="4.5"/><path d="m10.5 10.5 3 3"/>'),
  folder: stroke('<path d="M1.8 4.2c0-.7.5-1.2 1.2-1.2h3l1.5 1.7H13c.7 0 1.2.5 1.2 1.2v6.1c0 .7-.5 1.2-1.2 1.2H3c-.7 0-1.2-.5-1.2-1.2z"/>'),
  more: stroke('<circle cx="3.5" cy="8" r=".6" fill="currentColor"/><circle cx="8" cy="8" r=".6" fill="currentColor"/><circle cx="12.5" cy="8" r=".6" fill="currentColor"/>'),
  close: stroke('<path d="m4 4 8 8M12 4l-8 8"/>'),
  swap: stroke('<path d="M5 2.5v11M2.5 5 5 2.5 7.5 5M11 13.5v-11M8.5 11l2.5 2.5 2.5-2.5"/>'),
  eye: stroke('<path d="M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8z"/><circle cx="8" cy="8" r="2"/>'),
  edit: stroke('<path d="M10.8 2.7a1.6 1.6 0 0 1 2.3 2.3L6 12.2l-3 .8.8-3z"/>'),
  copy: stroke('<rect x="5" y="5" width="8.5" height="8.5" rx="1.5"/><path d="M11 5V3.7c0-.7-.5-1.2-1.2-1.2H3.7c-.7 0-1.2.5-1.2 1.2v6.1c0 .7.5 1.2 1.2 1.2H5"/>'),
  trash: stroke('<path d="M2.5 4.5h11M6.5 4.5V3c0-.3.2-.5.5-.5h2c.3 0 .5.2.5.5v1.5M4 4.5l.7 8.3c0 .4.4.7.8.7h5c.4 0 .8-.3.8-.7l.7-8.3"/>'),
  log: stroke('<path d="M3 3.5h10M3 6.5h10M3 9.5h7M3 12.5h5"/>'),
  download: stroke('<path d="M8 2.5v8M4.8 7.4 8 10.6l3.2-3.2M2.8 13.5h10.4"/>'),
  globe: stroke('<circle cx="8" cy="8" r="6"/><path d="M2 8h12M8 2c1.7 1.7 2.5 3.7 2.5 6S9.7 12.3 8 14C6.3 12.3 5.5 10.3 5.5 8S6.3 3.7 8 2z"/>'),
  code: stroke('<path d="M5.5 4.5 2 8l3.5 3.5M10.5 4.5 14 8l-3.5 3.5M9 3l-2 10"/>'),
  person: stroke('<circle cx="8" cy="5.3" r="2.7"/><path d="M2.8 13.8c.6-2.6 2.7-4.3 5.2-4.3s4.6 1.7 5.2 4.3"/>'),
  mail: stroke('<rect x="2" y="3.5" width="12" height="9" rx="1.5"/><path d="m2.5 4.5 5.5 4.3 5.5-4.3"/>'),
  warn: stroke('<path d="M7.1 2.6a1 1 0 0 1 1.8 0l5.4 9.6a1 1 0 0 1-.9 1.5H2.6a1 1 0 0 1-.9-1.5z"/><path d="M8 6.2v3.3"/><circle cx="8" cy="11.6" r=".5" fill="currentColor"/>'),
  lock: stroke('<rect x="3" y="7" width="10" height="7" rx="1.5"/><path d="M5.2 7V5.2a2.8 2.8 0 0 1 5.6 0V7"/>'),
  leave: stroke('<path d="M1.8 4.2c0-.7.5-1.2 1.2-1.2h3l1.5 1.7H13c.7 0 1.2.5 1.2 1.2v6.1c0 .7-.5 1.2-1.2 1.2H3c-.7 0-1.2-.5-1.2-1.2z"/><path d="m6 7.5 4 3.5M10 7.5 6 11"/>'),

  // The review: a checklist, for changes that wait for the person's OK
  review: stroke('<path d="m2.3 4.4 1.3 1.3L6 3.2M2.3 10l1.3 1.3L6 8.8M8.6 4.6h5M8.6 10.2h5"/>'),

  // Steps of a review, one per kind of change
  step: {
    new: stroke('<path d="M8 3.5v9M3.5 8h9"/>'),
    replace: stroke('<path d="M13 6.2A5.2 5.2 0 0 0 3.5 4.6M3 9.8a5.2 5.2 0 0 0 9.5 1.6"/><path d="M3.3 2.2v2.7H6M12.7 13.8v-2.7H10"/>'),
    remove: stroke('<path d="M3.5 8h9"/>'),
    folder: stroke('<path d="M1.8 4.2c0-.7.5-1.2 1.2-1.2h3l1.5 1.7H13c.7 0 1.2.5 1.2 1.2v6.1c0 .7-.5 1.2-1.2 1.2H3c-.7 0-1.2-.5-1.2-1.2z"/><path d="M8 7.2v3.6M6.2 9h3.6"/>'),
    "keep-both": stroke('<path d="M8 13.5V9.5C8 7.5 4.5 7.5 4.5 5V2.5M8 9.5c0-2 3.5-2 3.5-4.5V2.5"/>'),
    conflict: stroke('<path d="M6 6a2 2 0 1 1 2.9 1.8c-.6.3-.9.8-.9 1.4v.4"/><circle cx="8" cy="12.2" r=".5" fill="currentColor"/>'),
    settle: stroke('<path d="m3.5 8.3 2.8 2.8 6.2-6.4"/>'),
  },

  // Statuses: one icon each, always shown with its colour
  status: {
    "in-sync": stroke('<path d="m3.5 8.3 2.8 2.8 6.2-6.4"/>'),
    syncing: stroke('<path d="M13.5 8a5.5 5.5 0 0 1-9.6 3.7M2.5 8a5.5 5.5 0 0 1 9.6-3.7"/><path d="M12.4 1.8v2.6H9.8M3.6 14.2v-2.6h2.6"/>'),
    pending: stroke('<path d="M8 12.5v-9M4.5 7 8 3.5 11.5 7"/>'),
    paused: stroke('<path d="M5.5 3.5v9M10.5 3.5v9"/>'),
    conflict: stroke('<path d="M8 13.5V9M8 9 4 5M8 9l4-4M4 5V2.5M4 5H1.5M12 5V2.5M12 5h2.5"/>'),
    error: stroke('<path d="M8 3.5v5.5"/><circle cx="8" cy="12" r=".6" fill="currentColor"/>'),
    offline: stroke('<path d="M6 2.5v3M10 2.5v3M4.5 5.5h7v2.5a3.5 3.5 0 0 1-7 0zM8 11.5v2M2 2l12 12"/>'),
    new: stroke('<path d="M8 2.5v3M8 10.5v3M2.5 8h3M10.5 8h3M4.2 4.2l1.6 1.6M10.2 10.2l1.6 1.6M11.8 4.2l-1.6 1.6M5.8 10.2l-1.6 1.6"/>'),
    review: stroke('<path d="m2.3 4.4 1.3 1.3L6 3.2M2.3 10l1.3 1.3L6 8.8M8.6 4.6h5M8.6 10.2h5"/>'),
    looking: stroke('<circle cx="7" cy="7" r="4.2"/><path d="m10.1 10.1 3.4 3.4"/>'),
  },

  // Little diagrams for the three modes
  mode: {
    mirror: `<svg viewBox="0 0 80 34" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><rect x="2" y="7" width="20" height="20" rx="4"/><rect x="58" y="7" width="20" height="20" rx="4"/><path d="M28 17h22M45 12l5 5-5 5"/><path d="M63 13l10 8M73 13l-10 8" opacity=".55"/></svg>`,
    backup: `<svg viewBox="0 0 80 34" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><rect x="2" y="7" width="20" height="20" rx="4"/><rect x="58" y="7" width="20" height="20" rx="4"/><path d="M28 17h22M45 12l5 5-5 5"/><path d="M63 21h10M63 17h10M63 13h10" opacity=".55"/></svg>`,
    "two-way": `<svg viewBox="0 0 80 34" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><rect x="2" y="7" width="20" height="20" rx="4"/><rect x="58" y="7" width="20" height="20" rx="4"/><path d="M28 13h22M45 8l5 5-5 5M52 21H30M35 16l-5 5 5 5"/></svg>`,
  },
};

/** Fills every `[data-icon]` placeholder inside `root`. */
export function paintIcons(root = document) {
  for (const el of root.querySelectorAll("[data-icon]")) {
    el.innerHTML = Icons[el.dataset.icon] ?? "";
  }
}
