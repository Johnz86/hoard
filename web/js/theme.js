const KEY = "hoard.settings";
const THEMES = ["system", "dark", "light"];

const DEFAULTS = {
    theme: "system",
    density: "Condensed",
    roleColorMode: "Color-coded",
    showInspector: true,
    loadoutId: null
};

function read() {
    try {
        const raw = localStorage.getItem(KEY);
        if (!raw) return { ...DEFAULTS };
        return { ...DEFAULTS, ...JSON.parse(raw) };
    } catch {
        return { ...DEFAULTS };
    }
}

function write(settings) {
    try {
        localStorage.setItem(KEY, JSON.stringify(settings));
    } catch {
        return;
    }
}

export const settings = read();

export function applySettings() {
    const root = document.documentElement;
    if (settings.theme === "system") root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", settings.theme);
    root.setAttribute("data-density", settings.density === "Roomy" ? "roomy" : "condensed");
    root.setAttribute("data-rolecolor", settings.roleColorMode === "Monochrome" ? "mono" : "coded");
    root.setAttribute("data-inspector", settings.showInspector ? "on" : "off");
}

export function updateSettings(patch) {
    Object.assign(settings, patch);
    write(settings);
    applySettings();
    return settings;
}

export function cycleTheme() {
    const next = THEMES[(THEMES.indexOf(settings.theme) + 1) % THEMES.length];
    updateSettings({ theme: next });
    return next;
}

export function themeLabel() {
    return settings.theme.toUpperCase();
}
