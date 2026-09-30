import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useProfilerGridContext } from "../contexts/ProfilerGridContext";
import { Badge } from "./ui/badge";
import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbEllipsis,
  BreadcrumbLink,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
} from "./ui/breadcrumb";
import {
  HoverCard,
  HoverCardTrigger,
  HoverCardContent,
} from "./ui/hover-card";
import { cn } from "@/lib/utils.ts";

// ---------------------------------------------------------------------------
// Span classification
// ---------------------------------------------------------------------------

/** Map a span_context string to a kind tag used for colouring. */
function classifySpan(context: string | null | undefined): string | null {
  if (!context) return null;
  if (context === "clarity::builtin") return "builtin";
  if (context === "clarity::dispatch") return "dispatch";
  if (context.startsWith("clarity::user::")) {
    const visibility = context.slice("clarity::user::".length);
    if (visibility === "public") return "public";
    if (visibility === "private") return "private";
    if (visibility === "read-only" || visibility === "read_only") return "read-only";
    return "public"; // fallback for unknown user visibility
  }
  return null;
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/** Triple-arc Clarity icon (used for builtin + user functions) */
function ClarityIcon({ size = 12, className }: { size?: number; className?: string }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2.2"
      strokeLinecap="round"
      className={className}
    >
      <path d="M8 4a10 10 0 0 0 0 16" />
      <path d="M13 4a10 10 0 0 0 0 16" />
      <path d="M18 4a10 10 0 0 0 0 16" />
    </svg>
  );
}

/** Brace-arrow dispatch icon {→ */
function DispatchIcon({ size = 12, className }: { size?: number; className?: string }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2.2"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
    >
      {/* Opening brace */}
      <path d="M9 4c-2 0-3 1-3 3v3c0 2-1 2-2 2s2 0 2 2v3c0 2 1 3 3 3" />
      {/* Arrow pointing right */}
      <line x1="12" y1="12" x2="20" y2="12" />
      <polyline points="17 9 20 12 17 15" />
    </svg>
  );
}

function KindIcon({ kind, size = 12 }: { kind: string | null; size?: number }) {
  if (kind === "dispatch") return <DispatchIcon size={size} className="shrink-0" />;
  if (kind) return <ClarityIcon size={size} className="shrink-0" />;
  return null;
}

// ---------------------------------------------------------------------------
// Tailwind colour classes per kind
// ---------------------------------------------------------------------------

/** Badge colour classes for the kind chip. Uses CSS vars defined in styles.css */
const KIND_BADGE_CLASS: Record<string, string> = {
  builtin: "border-[color:color-mix(in_srgb,var(--span-builtin)_30%,transparent)] bg-[color:color-mix(in_srgb,var(--span-builtin)_10%,transparent)] text-[color:var(--span-builtin)]",
  public: "border-[color:color-mix(in_srgb,var(--span-public)_30%,transparent)] bg-[color:color-mix(in_srgb,var(--span-public)_10%,transparent)] text-[color:var(--span-public)]",
  private: "border-[color:color-mix(in_srgb,var(--span-private)_30%,transparent)] bg-[color:color-mix(in_srgb,var(--span-private)_10%,transparent)] text-[color:var(--span-private)]",
  "read-only": "border-[color:color-mix(in_srgb,var(--span-readonly)_30%,transparent)] bg-[color:color-mix(in_srgb,var(--span-readonly)_10%,transparent)] text-[color:var(--span-readonly)]",
  dispatch: "border-[color:color-mix(in_srgb,var(--span-dispatch)_30%,transparent)] bg-[color:color-mix(in_srgb,var(--span-dispatch)_10%,transparent)] text-[color:var(--span-dispatch)]",
};

/** Human-readable labels for each kind. Dispatch gets no text label (icon only). */
const KIND_LABEL: Record<string, string | null> = {
  builtin: "built-in",
  public: "public",
  private: "private",
  "read-only": "read-only",
  dispatch: null,
};

/** CSS class applied to span names for user-function kinds (coloured names). */
const NAME_CLASS: Record<string, string> = {
  public: "text-[color:var(--span-public)]",
  private: "text-[color:var(--span-private)]",
  "read-only": "text-[color:var(--span-readonly)]",
};

// ---------------------------------------------------------------------------
// Address shortening
// ---------------------------------------------------------------------------

const ADDR_RE = /(S[A-Z0-9]{3})[A-Z0-9]{20,}([A-Z0-9]{8})/g;

function shortenAddress(text: string): string {
  return text.replace(ADDR_RE, "$1\u2026$2");
}

// ---------------------------------------------------------------------------
// Metric formatting
// ---------------------------------------------------------------------------

function formatUs(us: number | null | undefined): string {
  if (us == null) return "-";
  const ms = us / 1000;
  if (ms >= 1000) return `${(ms / 1000).toFixed(2)}s`;
  if (ms >= 1) return `${ms.toFixed(2)}ms`;
  return `${us.toFixed(0)}µs`;
}

function formatCount(n: number | null | undefined): string {
  if (n == null) return "-";
  return Number(n).toLocaleString();
}

// ---------------------------------------------------------------------------
// SegmentMetricsCard — hover card content for a segment
// ---------------------------------------------------------------------------

function MetricRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex justify-between gap-4">
      <span className="text-muted-foreground">{label}</span>
      <span className="font-medium tabular-nums text-right">{value}</span>
    </div>
  );
}

interface SegmentData {
  name: string;
  tag?: string | null;
  span_context?: string;
  id?: string | number;
  call_count?: number | null;
  wall_us?: number | null;
  self_wall_us?: number | null;
  cpu_us?: number | null;
  self_cpu_us?: number | null;
}

function SegmentMetricsCard({ segment, kind }: { segment: SegmentData; kind: string | null }) {
  const hasMetrics = segment.wall_us != null || segment.call_count != null;
  if (!hasMetrics) return null;

  const label = kind ? KIND_LABEL[kind] ?? null : null;
  const badgeClass = kind ? KIND_BADGE_CLASS[kind] ?? "" : "";

  return (
    <div className="flex flex-col gap-2">
      {/* Header: kind chip + name */}
      <div className="flex items-center gap-1.5">
        {kind && (
          <Badge
            variant="outline"
            className={cn(
              "gap-0.5 px-1 py-0 text-[10px] leading-none font-semibold rounded-full",
              badgeClass
            )}
          >
            <KindIcon kind={kind} size={11} />
            {label && <span>{label}</span>}
          </Badge>
        )}
        <span className="text-sm font-semibold truncate">{segment.name}</span>
      </div>

      {segment.tag && (
        <div className="text-xs text-muted-foreground truncate">{segment.tag}</div>
      )}

      {/* Metrics grid */}
      <div className="flex flex-col gap-0.5 text-xs">
        {segment.call_count != null && (
          <MetricRow label="Calls" value={formatCount(segment.call_count)} />
        )}
        {segment.wall_us != null && (
          <MetricRow label="Wall" value={formatUs(segment.wall_us)} />
        )}
        {segment.self_wall_us != null && (
          <MetricRow label="Self wall" value={formatUs(segment.self_wall_us)} />
        )}
        {segment.cpu_us != null && (
          <MetricRow label="CPU" value={formatUs(segment.cpu_us)} />
        )}
        {segment.self_cpu_us != null && (
          <MetricRow label="Self CPU" value={formatUs(segment.self_cpu_us)} />
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// SpanSegmentContent — inner content for a single breadcrumb item
// ---------------------------------------------------------------------------

/**
 * Renders the content of a single breadcrumb segment:
 *   [KindBadge] name [TagBadge]
 *
 * Used inside both BreadcrumbLink (clickable) and BreadcrumbPage (current).
 */
function SpanSegmentContent({ segment, kind }: { segment: SegmentData; kind: string | null }) {
  const label = kind ? KIND_LABEL[kind] ?? null : null;
  const nameClass = kind ? NAME_CLASS[kind] ?? "" : "";
  const badgeClass = kind ? KIND_BADGE_CLASS[kind] ?? "" : "";
  const shortName = shortenAddress(segment.name);

  return (
    <span className="inline-flex items-center gap-1">
      {/* Kind chip */}
      {kind && (
        <Badge
          variant="outline"
          className={cn(
            "gap-0.5 px-1 py-0 text-[10px] leading-none font-semibold rounded-full",
            badgeClass
          )}
        >
          <KindIcon kind={kind} size={11} />
          {label && <span>{label}</span>}
        </Badge>
      )}

      {/* Span name */}
      <span className={cn("whitespace-nowrap", nameClass)}>
        {shortName}
      </span>

      {/* Per-segment tag shown as a tiny outline badge */}
      {segment.tag ? (
        <Badge
          variant="outline"
          className="px-1 py-0 text-[10px] leading-none font-normal rounded-full text-primary border-primary/20 bg-primary/5"
        >
          {shortenAddress(segment.tag)}
        </Badge>
      ) : null}
    </span>
  );
}

// ---------------------------------------------------------------------------
// Responsive breadcrumb collapse hook
// ---------------------------------------------------------------------------

/**
 * Observes a breadcrumb container and determines how many middle segments
 * to collapse so that the first and last segments remain visible.
 *
 * Strategy:
 *  - When collapsed is null, all items render; useLayoutEffect measures each
 *    item's width and caches it.  If they overflow, a collapse range is set.
 *  - When collapsed is non-null, collapsed items are NOT rendered (replaced by
 *    an ellipsis).  A ResizeObserver recalculates with cached widths on resize.
 *  - If the container grows enough, collapsed resets to null → a full render
 *    re-measures before paint (useLayoutEffect), so users never see a flash.
 *
 * Returns [listRef, collapsedRange].
 */
interface CollapsedRange {
  start: number;
  end: number;
}

function useCollapsedBreadcrumb(segmentCount: number): [React.RefObject<HTMLOListElement | null>, CollapsedRange | null] {
  const listRef = useRef<HTMLOListElement>(null);
  const [collapsed, setCollapsed] = useState<CollapsedRange | null>(null);
  const widthsRef = useRef<number[]>([]);

  // Compute collapse range from cached widths
  const recalc = useCallback(() => {
    const list = listRef.current;
    if (!list || segmentCount <= 2) {
      setCollapsed(null);
      return;
    }

    const widths = widthsRef.current;
    if (widths.length !== segmentCount) return; // not measured yet

    const containerWidth = list.parentElement?.clientWidth ?? list.clientWidth;
    const totalWidth = widths.reduce((a, b) => a + b, 0);

    if (totalWidth <= containerWidth) {
      setCollapsed((prev: CollapsedRange | null) => (prev === null ? null : null)); // force null
      return;
    }

    const firstW = widths[0];
    const lastW = widths[segmentCount - 1];
    const ellipsisW = 40; // approx width of "⋯" + separator
    const available = containerWidth - firstW - lastW - ellipsisW;

    if (available <= 0) {
      setCollapsed({ start: 1, end: segmentCount - 2 });
      return;
    }

    // Keep middle items from the END (closest to the leaf span)
    let usedWidth = 0;
    let keepFromEnd = 0;
    for (let i = segmentCount - 2; i >= 1; i--) {
      if (usedWidth + widths[i] > available) break;
      usedWidth += widths[i];
      keepFromEnd++;
    }

    const collapseEnd = segmentCount - 2 - keepFromEnd;
    setCollapsed(collapseEnd >= 1 ? { start: 1, end: collapseEnd } : null);
  }, [segmentCount]);

  // Measure item widths when everything is visible (collapsed === null)
  useLayoutEffect(() => {
    const list = listRef.current;
    if (!list || collapsed !== null) return;

    const children = Array.from(list.children) as HTMLElement[];
    // DOM order: [item0, sep0, item1, sep1, ..., itemN-1]
    const widths = [];
    for (let i = 0; i < segmentCount; i++) {
      const itemEl = children[i * 2] as HTMLElement | undefined;
      const sepEl = i < segmentCount - 1 ? children[i * 2 + 1] as HTMLElement | undefined : null;
      const gap = 4; // gap-1 = 0.25rem ≈ 4px
      widths.push(
        (itemEl?.offsetWidth ?? 0) + (sepEl?.offsetWidth ?? 0) + gap
      );
    }
    widthsRef.current = widths;
    recalc();
  }, [collapsed, segmentCount, recalc]);

  // Recalculate on resize
  useEffect(() => {
    const el = listRef.current?.parentElement;
    if (!el) return;
    const ro = new ResizeObserver(() => recalc());
    ro.observe(el);
    return () => ro.disconnect();
  }, [recalc]);

  return [listRef, collapsed];
}

// ---------------------------------------------------------------------------
// Main component
// ---------------------------------------------------------------------------

export default function SpanCell({ row }: { row: Record<string, any> }) {
  const { toggleChain, expandChainTo, focusNode, spanVizConfig, getSpanVizValue } =
    useProfilerGridContext();
  const cellRef = useRef<HTMLDivElement>(null);

  const percent = row.flame_percent ?? 0;
  const chainCount = row.chain_count ?? 0;
  const hiddenSiblings = row.hidden_siblings ?? 0;
  const segments = row.chain_segments;
  const isChain = Array.isArray(segments) && segments.length > 1;

  const [breadcrumbListRef, collapsedRange] = useCollapsedBreadcrumb(
    isChain ? segments.length : 0
  );

  // --- Span viz effect ---
  useEffect(() => {
    if (!cellRef.current) return;
    const wxCell = cellRef.current.closest(".wx-cell") as HTMLElement | null;
    if (!wxCell) return;

    if (!spanVizConfig?.enabled) {
      wxCell.style.setProperty("--span-viz-width", "0");
      wxCell.style.setProperty("--span-viz-alpha", "0");
      return;
    }

    const { level, pct } = getSpanVizValue?.(row) ?? { level: 0, pct: 0 };
    const alpha = level > 0 ? 0.04 + level * 0.2 : 0;

    wxCell.style.setProperty("--span-viz-width", String(pct));
    wxCell.style.setProperty("--span-viz-alpha", String(alpha));
  }, [row, spanVizConfig, getSpanVizValue]);

  // --- Determine kind for a single (non-chain) row ---
  const singleKind = !isChain ? classifySpan(row.span_context) : null;

  return (
    <div className="span-cell" ref={cellRef}>
      <div className="span-label">
        {/* Flame percentage */}
        <span className="span-percent">{percent.toFixed(1)}%</span>

        {/* ── Chain breadcrumb rendering ── */}
        {isChain ? (
          <Breadcrumb className="min-w-0 flex-1 overflow-hidden">
            <BreadcrumbList ref={breadcrumbListRef} className="flex-nowrap gap-1 text-xs sm:gap-1">
              {segments.map((seg, i) => {
                const kind = classifySpan(seg.span_context);
                const isLast = i === segments.length - 1;
                const isCollapsed =
                  collapsedRange != null &&
                  i >= collapsedRange.start &&
                  i <= collapsedRange.end;

                // ── Ellipsis: render once at collapse start ──
                if (isCollapsed && i === collapsedRange.start) {
                  const collapsedSegs = segments.slice(
                    collapsedRange.start,
                    collapsedRange.end + 1
                  );
                  return (
                    <React.Fragment key={`ellipsis-${i}`}>
                      <BreadcrumbItem>
                        <HoverCard openDelay={300} closeDelay={200}>
                          <HoverCardTrigger asChild>
                            <button
                              type="button"
                              className="inline-flex items-center"
                              onClick={(e) => {
                                e.stopPropagation();
                                expandChainTo?.(segments, collapsedRange.end + 1);
                              }}
                            >
                              <BreadcrumbEllipsis className="h-4 w-4" />
                              <span className="sr-only">
                                {collapsedSegs.length} collapsed segments
                              </span>
                            </button>
                          </HoverCardTrigger>
                          <HoverCardContent side="bottom" align="start" className="w-72 p-3">
                            <div className="flex flex-col gap-1.5">
                              <span className="text-xs font-medium text-muted-foreground">
                                {collapsedSegs.length} collapsed segment{collapsedSegs.length > 1 ? "s" : ""}
                              </span>
                              {collapsedSegs.map((cs, ci) => {
                                const ck = classifySpan(cs.span_context);
                                return (
                                  <div key={cs.id ?? ci} className="flex items-center gap-1 text-xs">
                                    <KindIcon kind={ck} size={10} />
                                    <span className={cn("truncate", ck ? NAME_CLASS[ck] ?? "" : "")}>
                                      {shortenAddress(cs.name)}
                                    </span>
                                    {cs.tag && (
                                      <span className="text-muted-foreground truncate">
                                        {shortenAddress(cs.tag)}
                                      </span>
                                    )}
                                  </div>
                                );
                              })}
                            </div>
                          </HoverCardContent>
                        </HoverCard>
                      </BreadcrumbItem>
                      <BreadcrumbSeparator className="[&>svg]:w-3 [&>svg]:h-3" />
                    </React.Fragment>
                  );
                }

                // ── Skip other collapsed items ──
                if (isCollapsed) return null;

                // ── Normal visible segment ──
                return (
                  <React.Fragment key={seg.id ?? i}>
                    <BreadcrumbItem className="gap-1">
                      <HoverCard openDelay={400} closeDelay={200}>
                        <HoverCardTrigger asChild>
                          {isLast ? (
                            <BreadcrumbPage className="inline-flex items-center gap-1">
                              <SpanSegmentContent segment={seg} kind={kind} />
                            </BreadcrumbPage>
                          ) : (
                            <BreadcrumbLink
                              asChild
                              className="inline-flex items-center gap-1 cursor-pointer"
                            >
                              <button
                                type="button"
                                onClick={(e) => {
                                  e.stopPropagation();
                                  expandChainTo?.(segments, i + 1);
                                }}
                              >
                                <SpanSegmentContent segment={seg} kind={kind} />
                              </button>
                            </BreadcrumbLink>
                          )}
                        </HoverCardTrigger>
                        <HoverCardContent side="bottom" align="start" className="w-72 p-3">
                          <SegmentMetricsCard segment={seg} kind={kind} />
                        </HoverCardContent>
                      </HoverCard>
                    </BreadcrumbItem>
                    {!isLast && <BreadcrumbSeparator className="[&>svg]:w-3 [&>svg]:h-3" />}
                  </React.Fragment>
                );
              })}
            </BreadcrumbList>
          </Breadcrumb>
        ) : (
          /* ── Single span rendering ── */
          <HoverCard openDelay={400} closeDelay={200}>
            <HoverCardTrigger asChild>
              <span className="inline-flex items-center gap-1">
                <SpanSegmentContent
                  segment={{ name: row.span_name ?? "-", tag: row.tag ?? null }}
                  kind={singleKind}
                />
              </span>
            </HoverCardTrigger>
            <HoverCardContent side="bottom" align="start" className="w-72 p-3">
              <SegmentMetricsCard
                segment={{
                  name: row.span_name ?? "-",
                  tag: row.tag ?? null,
                  call_count: row.call_count ?? null,
                  wall_us: row.est_wall_us ?? row.wall_time_us ?? null,
                  self_wall_us: row.est_self_wall_us ?? row.self_wall_time_us ?? null,
                  cpu_us: row.est_cpu_us ?? row.cpu_time_us ?? null,
                  self_cpu_us: row.est_self_cpu_us ?? row.self_cpu_time_us ?? null,
                }}
                kind={singleKind}
              />
            </HoverCardContent>
          </HoverCard>
        )}

        {/* Compressed-chain frame count toggle */}
        {chainCount > 0 && (
          <Badge
            variant="secondary"
            className="cursor-pointer px-1.5 py-0 text-[10px] leading-relaxed font-normal rounded-full text-muted-foreground"
            onClick={(e) => {
              e.stopPropagation();
              toggleChain?.(row.id);
            }}
          >
            +{chainCount} frames
          </Badge>
        )}

        {/* Hot-path hidden siblings count */}
        {hiddenSiblings > 0 && (
          <Badge
            variant="secondary"
            className="px-1.5 py-0 text-[10px] leading-relaxed font-normal rounded-full text-muted-foreground"
          >
            +{hiddenSiblings} siblings
          </Badge>
        )}

        {/* Filtered children indicator */}
        {(row._filtered_children ?? 0) > 0 && (
          <Badge
            variant="secondary"
            className="px-1.5 py-0 text-[10px] leading-relaxed font-normal rounded-full text-muted-foreground"
          >
            {row._filtered_children} filtered
          </Badge>
        )}

        {/* Focus button */}
        <button
          type="button"
          className="span-focus-btn"
          onClick={(e) => {
            e.stopPropagation();
            focusNode?.(row.id);
          }}
        >
          Focus
        </button>
      </div>
    </div>
  );
}
