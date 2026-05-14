"use client";

import { useCallback, useEffect, useRef, type MouseEvent as ReactMouseEvent } from "react";
import ReactFlow, {
  Background,
  ConnectionMode,
  ConnectionLineType,
  Controls,
  MiniMap,
  type EdgeMouseHandler,
  type NodeMouseHandler,
  type NodeTypes,
  type Connection,
  type EdgeChange,
  type NodeChange,
  type ReactFlowInstance
} from "reactflow";
import "reactflow/dist/style.css";
import type { FlowEdge, FlowNode } from "@/components/editor/types";

type PaneMouseHandler = (event: ReactMouseEvent<Element, MouseEvent>) => void;

interface Props {
  nodes: FlowNode[];
  edges: FlowEdge[];
  onNodesChange: (changes: NodeChange[]) => void;
  onEdgesChange: (changes: EdgeChange[]) => void;
  onConnect: (connection: Connection) => void;
  onSelectionChange: (payload: { nodes: FlowNode[]; edges: FlowEdge[] }) => void;
  onEdgeDoubleClick?: EdgeMouseHandler;
  onEdgeContextMenu?: EdgeMouseHandler;
  onNodeClick?: (nodeId: string) => void;
  onEdgeClick?: (edgeId: string) => void;
  onPaneClick?: () => void;
  onPaneDoubleClick?: PaneMouseHandler;
  nodeTypes?: NodeTypes;
  connectionModeEnabled?: boolean;
  readOnly?: boolean;
  allowNodeDrag?: boolean;
  onInit?: (instance: ReactFlowInstance) => void;
}

export function EditorCanvas({
  nodes,
  edges,
  onNodesChange,
  onEdgesChange,
  onConnect,
  onSelectionChange,
  onEdgeDoubleClick,
  onEdgeContextMenu,
  onNodeClick,
  onEdgeClick,
  onPaneClick,
  onPaneDoubleClick,
  nodeTypes,
  connectionModeEnabled = true,
  readOnly = false,
  allowNodeDrag = false,
  onInit
}: Props) {
  const flowInstanceRef = useRef<ReactFlowInstance | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const minZoom = 0.25;
  const maxZoom = 2;

  const handleInit = useCallback(
    (instance: ReactFlowInstance) => {
      flowInstanceRef.current = instance;
      onInit?.(instance);
    },
    [onInit]
  );

  const handleAltWheelZoom = useCallback((event: WheelEvent) => {
    if (!event.altKey) {
      return;
    }
    const instance = flowInstanceRef.current;
    if (!instance) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    const currentZoom = instance.getZoom();
    const zoomStep = event.deltaY < 0 ? 0.1 : -0.1;
    const nextZoom = Math.max(minZoom, Math.min(maxZoom, currentZoom + zoomStep));
    instance.zoomTo(nextZoom, { duration: 80 });
  }, []);

  const handleNodeClick = useCallback<NodeMouseHandler>(
    (_event, node) => {
      onNodeClick?.(node.id);
    },
    [onNodeClick]
  );

  const handleEdgeClick = useCallback<EdgeMouseHandler>(
    (_event, edge) => {
      onEdgeClick?.(edge.id);
    },
    [onEdgeClick]
  );

  const handlePaneClick = useCallback(() => {
    onPaneClick?.();
  }, [onPaneClick]);

  const handleEdgeContextMenu = useCallback<EdgeMouseHandler>(
    (event, edge) => {
      onEdgeContextMenu?.(event, edge);
    },
    [onEdgeContextMenu]
  );

  useEffect(() => {
    const handleWindowWheel = (event: WheelEvent) => {
      const container = containerRef.current;
      if (!container) {
        return;
      }
      if (!(event.target instanceof Node) || !container.contains(event.target)) {
        return;
      }
      handleAltWheelZoom(event);
    };

    window.addEventListener("wheel", handleWindowWheel, {
      passive: false,
      capture: true
    });

    return () => {
      window.removeEventListener("wheel", handleWindowWheel, true);
    };
  }, [handleAltWheelZoom]);

  return (
    <div
      ref={containerRef}
      role="application"
      aria-label="Editor visual de topología DKMS"
      className="h-[60vh] min-h-[420px] overflow-hidden rounded-xl border border-border bg-gradient-to-br from-card via-card to-muted/40 shadow-sm sm:h-[65vh] lg:h-[calc(100vh-14rem)]"
      onDoubleClick={(event) => {
        const target = event.target;
        if (!(target instanceof Element)) {
          return;
        }
        if (!target.closest(".react-flow__pane")) {
          return;
        }
        onPaneDoubleClick?.(event);
      }}
    >
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onConnect={onConnect}
        onSelectionChange={onSelectionChange}
        onEdgeDoubleClick={onEdgeDoubleClick}
        onEdgeContextMenu={handleEdgeContextMenu}
        onNodeClick={handleNodeClick}
        onEdgeClick={handleEdgeClick}
        onPaneClick={handlePaneClick}
        nodeTypes={nodeTypes}
        connectionMode={ConnectionMode.Loose}
        nodesConnectable={connectionModeEnabled && !readOnly}
        nodesDraggable={allowNodeDrag && !readOnly}
        edgesUpdatable={allowNodeDrag && !readOnly}
        elementsSelectable
        multiSelectionKeyCode={["Meta", "Control"]}
        selectionKeyCode={["Shift", "Meta", "Control"]}
        onInit={handleInit}
        connectionLineType={ConnectionLineType.Straight}
        defaultEdgeOptions={{ type: "straight" }}
        fitView
        fitViewOptions={{ padding: 0.25, duration: 250 }}
        minZoom={minZoom}
        maxZoom={maxZoom}
        zoomOnScroll={false}
        zoomOnPinch={false}
        panOnScroll={false}
        preventScrolling={false}
      >
        <Background gap={18} size={1} />
        <MiniMap zoomable pannable />
        <Controls />
      </ReactFlow>
    </div>
  );
}
