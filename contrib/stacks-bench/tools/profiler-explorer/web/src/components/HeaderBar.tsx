import React from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Sun, Moon, RotateCcw, Settings, Loader2, X, Square, GitGraph, List } from "lucide-react";

interface RunInfo {
  id: number;
  run_name?: string;
}

interface HeaderBarProps {
  runs: RunInfo[];
  runId: string;
  setRunId: (id: string) => void;
  txQuery: string;
  setTxQuery: (q: string) => void;
  mode: string;
  rowsLength: number;
  theme: string;
  toggleTheme: () => void;
  resetQuery: () => void;
  onOpenSettings: () => void;
  loadTrace: () => void;
  cancelLoad: () => void;
  isDirty: boolean;
  isLoading: boolean;
  activeTab: string;
  setActiveTab: (tab: string) => void;
}

export default function HeaderBar({
  runs,
  runId,
  setRunId,
  txQuery,
  setTxQuery,
  mode,
  rowsLength,
  theme,
  toggleTheme,
  resetQuery,
  onOpenSettings,
  loadTrace,
  cancelLoad,
  isDirty,
  isLoading,
  activeTab,
  setActiveTab,
}: HeaderBarProps) {
  const canSearch = txQuery.length === 64 && /^[0-9a-fA-F]{64}$/.test(txQuery);
  
  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" && canSearch) {
      e.preventDefault();
      loadTrace();
    }
  };
  
  return (
    <header className="app-header">
      <div className="header-left">
        <h1 className="header-title">Profiler Explorer</h1>
        <div className="header-divider" />
        <Select value={runId} onValueChange={(v) => { if (v !== null) setRunId(v); }}>
          <SelectTrigger className="w-[200px]">
            <SelectValue placeholder="Select a run" />
          </SelectTrigger>
          <SelectContent>
            {runs.map((run) => (
              <SelectItem key={run.id} value={String(run.id)}>
                {run.run_name ? `${run.id} · ${run.run_name}` : `Run ${run.id}`}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <div className="header-divider" />
        <div className="header-tabs">
          <button
            type="button"
            className={`header-tab ${activeTab === "transactions" ? "header-tab-active" : ""}`}
            onClick={() => setActiveTab("transactions")}
          >
            <List className="h-4 w-4" />
            Transactions
          </button>
          <button
            type="button"
            className={`header-tab ${activeTab === "trace" ? "header-tab-active" : ""}`}
            onClick={() => setActiveTab("trace")}
          >
            <GitGraph className="h-4 w-4" />
            Trace
          </button>
        </div>
      </div>

      <div className="header-center">
        <div
          className={`search-container ${
            txQuery && !/^[0-9a-fA-F]{0,64}$/.test(txQuery)
              ? "search-container-invalid"
              : ""
          }`}
        >
          <svg
            className="search-icon"
            width="16"
            height="16"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.5"
          >
            <circle cx="7" cy="7" r="5" />
            <path d="M11 11l3 3" />
          </svg>
          <input
            type="text"
            className="search-input"
            value={txQuery}
            onChange={(e) => {
              const value = e.target.value.trim();
              if (value === "" || /^[0-9a-fA-F]{0,64}$/.test(value)) {
                setTxQuery(value);
              }
            }}
            onKeyDown={handleKeyDown}
            placeholder="Enter transaction hash (64 hex chars)..."
            maxLength={64}
            spellCheck={false}
          />
          {txQuery && (
            <>
              <span
                className={`search-length ${txQuery.length === 64 ? "search-length-valid" : ""}`}
              >
                {txQuery.length}/64
              </span>
              <Button
                variant="ghost"
                size="icon"
                className="h-6 w-6 mr-1"
                onClick={() => setTxQuery("")}
              >
                <X className="h-3 w-3" />
              </Button>
            </>
          )}
        </div>
      </div>

      <div className="header-right">
        {activeTab === "trace" && (
          <div className="header-stats">
            <Badge variant="secondary" className="text-xs font-medium">
              {rowsLength.toLocaleString()} rows
            </Badge>
          </div>
        )}
        <TooltipProvider delayDuration={300}>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button variant="outline" size="icon" onClick={toggleTheme}>
                {theme === "dark" ? (
                  <Sun className="h-4 w-4" />
                ) : (
                  <Moon className="h-4 w-4" />
                )}
              </Button>
            </TooltipTrigger>
            <TooltipContent>
              {theme === "dark" ? "Switch to light mode" : "Switch to dark mode"}
            </TooltipContent>
          </Tooltip>
        </TooltipProvider>
        <TooltipProvider delayDuration={300}>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button variant="outline" size="icon" onClick={onOpenSettings}>
                <Settings className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Settings</TooltipContent>
          </Tooltip>
        </TooltipProvider>
        {activeTab === "trace" && (
          <>
            <TooltipProvider delayDuration={300}>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button variant="outline" size="icon" onClick={resetQuery}>
                    <RotateCcw className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Reset all filters</TooltipContent>
              </Tooltip>
            </TooltipProvider>
            {isLoading ? (
              <Button variant="destructive" onClick={cancelLoad}>
                <Square className="h-4 w-4 mr-2" />
                Cancel
              </Button>
            ) : (
              <Button onClick={loadTrace} disabled={!isDirty}>
                Load Trace
              </Button>
            )}
          </>
        )}
      </div>
    </header>
  );
}
