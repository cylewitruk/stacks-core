import React, { useEffect, useRef } from "react";

interface BlockInfo {
  stacks_block_id: string;
  height: number;
  block_hash_hex?: string;
}

interface FormatOption {
  id: string;
  label: string;
}

interface SettingsPanelProps {
  open: boolean;
  onClose: () => void;
  mode: string;
  setMode: (mode: string) => void;
  minWallMs: string;
  setMinWallMs: (val: string) => void;
  limit: string;
  setLimit: (val: string) => void;
  hotPathMode: string;
  setHotPathMode: (mode: string) => void;
  chainCompression: boolean;
  setChainCompression: (val: boolean) => void;
  numberFormatId: string;
  setNumberFormatId: (id: string) => void;
  themePreset: string;
  setThemePreset: (id: string) => void;
  segmentRootId: string;
  setSegmentRootId: (id: string) => void;
  stacksBlockId: string;
  setStacksBlockId: (id: string) => void;
  blocks: BlockInfo[];
  numberFormats: FormatOption[];
  themePresets: FormatOption[];
}

export default function SettingsPanel({
  open,
  onClose,
  mode,
  setMode,
  minWallMs,
  setMinWallMs,
  limit,
  setLimit,
  hotPathMode,
  setHotPathMode,
  chainCompression,
  setChainCompression,
  numberFormatId,
  setNumberFormatId,
  themePreset,
  setThemePreset,
  segmentRootId,
  setSegmentRootId,
  stacksBlockId,
  setStacksBlockId,
  blocks,
  numberFormats,
  themePresets,
}: SettingsPanelProps) {
  const panelRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleEscape = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    if (open) {
      document.addEventListener("keydown", handleEscape);
      return () => document.removeEventListener("keydown", handleEscape);
    }
  }, [open, onClose]);

  return (
    <>
      <div
        className={`settings-backdrop ${open ? "settings-backdrop-open" : ""}`}
        onClick={onClose}
      />
      <div
        ref={panelRef}
        className={`settings-panel ${open ? "settings-panel-open" : ""}`}
      >
        <div className="settings-panel-header">
          <h2 className="text-sm font-semibold text-foreground">Settings</h2>
          <button
            type="button"
            className="settings-close-btn"
            onClick={onClose}
            aria-label="Close settings"
          >
            <svg
              width="16"
              height="16"
              viewBox="0 0 16 16"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
            >
              <path d="M4 4l8 8M12 4l-8 8" />
            </svg>
          </button>
        </div>

        <div className="settings-panel-content">
          <div className="settings-section">
            <h3 className="settings-section-title">Query Mode</h3>
            <div className="settings-mode-toggle">
              <button
                type="button"
                className={`settings-mode-btn ${mode === "tx" ? "settings-mode-btn-active" : ""}`}
                onClick={() => setMode("tx")}
              >
                Transaction
              </button>
              <button
                type="button"
                className={`settings-mode-btn ${mode === "run" ? "settings-mode-btn-active" : ""}`}
                onClick={() => setMode("run")}
              >
                Run Scope
              </button>
            </div>
          </div>

          <div className="settings-section">
            <h3 className="settings-section-title">Query Filters</h3>
            <div className="settings-field">
              <label className="settings-label">Min Wall Time (ms)</label>
              <input
                type="text"
                className="settings-input"
                value={minWallMs}
                onChange={(e) => setMinWallMs(e.target.value)}
                placeholder="e.g. 1"
              />
            </div>
            <div className="settings-field">
              <label className="settings-label">Record Limit</label>
              <input
                type="text"
                className="settings-input"
                value={limit}
                onChange={(e) => setLimit(e.target.value)}
                placeholder="5000"
              />
            </div>
            {mode === "run" && (
              <>
                <div className="settings-field">
                  <label className="settings-label">Stacks Block</label>
                  <select
                    className="settings-input"
                    value={stacksBlockId}
                    onChange={(e) => setStacksBlockId(e.target.value)}
                  >
                    <option value="">Any</option>
                    {blocks.map((block) => (
                      <option
                        key={block.stacks_block_id}
                        value={block.stacks_block_id}
                      >
                        #{block.height} · {block.block_hash_hex?.slice(0, 12)}...
                      </option>
                    ))}
                  </select>
                </div>
                <div className="settings-field">
                  <label className="settings-label">Segment Root ID</label>
                  <input
                    type="text"
                    className="settings-input"
                    value={segmentRootId}
                    onChange={(e) => setSegmentRootId(e.target.value)}
                    placeholder="Optional"
                  />
                </div>
              </>
            )}
          </div>

          <div className="settings-section">
            <h3 className="settings-section-title">Display</h3>
            <div className="settings-field">
              <label className="settings-label">Hot Path</label>
              <select
                className="settings-input"
                value={hotPathMode}
                onChange={(e) => setHotPathMode(e.target.value)}
              >
                <option value="off">Off</option>
                <option value="inclusive">Inclusive Time</option>
                <option value="self">Self Time</option>
              </select>
            </div>
            <div className="settings-field">
              <label className="settings-label">Number Format</label>
              <select
                className="settings-input"
                value={numberFormatId}
                onChange={(e) => setNumberFormatId(e.target.value)}
              >
                {numberFormats.map((format) => (
                  <option key={format.id} value={format.id}>
                    {format.label}
                  </option>
                ))}
              </select>
            </div>
            <div className="settings-field">
              <label className="settings-label">Theme</label>
              <select
                className="settings-input"
                value={themePreset}
                onChange={(e) => setThemePreset(e.target.value)}
              >
                {themePresets.map((preset) => (
                  <option key={preset.id} value={preset.id}>
                    {preset.label}
                  </option>
                ))}
              </select>
            </div>
            <label className="settings-checkbox">
              <input
                type="checkbox"
                checked={chainCompression}
                onChange={(e) => setChainCompression(e.target.checked)}
              />
              <span>Compress linear chains</span>
            </label>
          </div>
        </div>
      </div>
    </>
  );
}
