import React from "react";

interface BreadcrumbNode {
  id: string | number;
  span_name?: string;
}

export default function BreadcrumbBar({ breadcrumb }: { breadcrumb: BreadcrumbNode[] }) {
  if (!breadcrumb || breadcrumb.length === 0) return null;
  return (
    <div className="breadcrumb-bar">
      <span className="breadcrumb-label">Focus:</span>
      {breadcrumb.map((node: BreadcrumbNode, index: number) => (
        <span key={node.id} className="breadcrumb-item">
          {node.span_name ?? `Node ${node.id}`}
          {index < breadcrumb.length - 1 && <span className="breadcrumb-separator">›</span>}
        </span>
      ))}
    </div>
  );
}
