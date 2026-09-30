export const DEFAULT_AUTO_EXPAND = {
  depth: 2,
  selfMs: 5,
  wallMs: 20,
  topKChildren: 3,
};

export const NUMBER_FORMATS = [
  { id: "space-dot", label: "1 000 000.00", group: " ", decimal: "." },
  { id: "dot-comma", label: "1.000.000,00", group: ".", decimal: "," },
  { id: "comma-dot", label: "1,000,000.00", group: ",", decimal: "." },
];

export const DEFAULT_NUMBER_FORMAT_ID = "comma-dot";
export const DEFAULT_HEAT_STYLE = "fill";
export const DEFAULT_HEAT_COLOR = "red";

export const HEAT_COLOR_OPTIONS = [
  { id: "red", label: "Red" },
  { id: "orange", label: "Orange" },
  { id: "amber", label: "Amber" },
  { id: "green", label: "Green" },
  { id: "teal", label: "Teal" },
  { id: "cyan", label: "Cyan" },
  { id: "blue", label: "Blue" },
  { id: "purple", label: "Purple" },
  { id: "pink", label: "Pink" },
  { id: "gray", label: "Gray" },
];

export const THEME_PRESETS = [
  { id: "default", label: "Default" },
  { id: "ocean", label: "Ocean" },
  { id: "grape", label: "Grape" },
  { id: "ember", label: "Ember" },
];
