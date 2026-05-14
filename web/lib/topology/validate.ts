import type { SimulationLinkInput, SimulationNodeInput } from "@/lib/topology/types";

export interface TopologyValidationResult {
  errors: string[];
  warnings: string[];
}

export function validateSimulationGraph(nodes: SimulationNodeInput[], links: SimulationLinkInput[]): TopologyValidationResult {
  const errors: string[] = [];
  const warnings: string[] = [];

  const classicLinks = links.filter((link) => String(link.linkType) === "CLASSIC");
  if (classicLinks.length > 0) {
    errors.push("CLASSIC links are not supported. Allowed: QKD, PQC, HYBRID.");
  }

  const dkmsNodeIds = nodes
    .filter((node) => node.type === "DKMS")
    .map((node) => node.nodeId)
    .filter((value): value is number => typeof value === "number");

  const duplicates = dkmsNodeIds.filter((value, index) => dkmsNodeIds.indexOf(value) !== index);
  if (duplicates.length > 0) {
    const uniqueDuplicates = [...new Set(duplicates)].sort((a, b) => a - b);
    errors.push(`Duplicate DKMS node_id values detected: ${uniqueDuplicates.join(", ")}`);
  }

  const selfLinks = links.filter((link) => link.sourceUid === link.targetUid);
  if (selfLinks.length > 0) {
    errors.push(`Self-links are not allowed (${selfLinks.length} found).`);
  }

  const undirectedPairs = new Set<string>();
  const duplicatePairs = new Set<string>();
  for (const link of links) {
    const key =
      link.sourceUid < link.targetUid ? `${link.sourceUid}::${link.targetUid}` : `${link.targetUid}::${link.sourceUid}`;
    if (undirectedPairs.has(key)) {
      duplicatePairs.add(key);
    } else {
      undirectedPairs.add(key);
    }
  }
  if (duplicatePairs.size > 0) {
    errors.push(`Only one link is allowed between the same two nodes (${duplicatePairs.size} duplicated pairs).`);
  }

  const invalidDistances = links.filter(
    (link) =>
      link.linkType !== "PQC" &&
      (!Number.isFinite(Number(link.distanceKm)) || Number(link.distanceKm) < 0)
  );
  if (invalidDistances.length > 0) {
    errors.push(`All links must have distance_km >= 0 (${invalidDistances.length} invalid).`);
  }

  const invalidQudittoMaxBuffer = links.filter(
    (link) =>
      link.linkType !== "PQC" &&
      (!Number.isFinite(Number(link.qudittoMaxBufferSize)) || Number(link.qudittoMaxBufferSize) < 1)
  );
  if (invalidQudittoMaxBuffer.length > 0) {
    errors.push(
      `All links must have quditto_max_buffer_size >= 1 (${invalidQudittoMaxBuffer.length} invalid).`
    );
  }

  const invalidQudittoRateR0 = links.filter(
    (link) =>
      link.linkType !== "PQC" &&
      (!Number.isFinite(Number(link.qudittoRateR0)) || Number(link.qudittoRateR0) <= 0)
  );
  if (invalidQudittoRateR0.length > 0) {
    errors.push(`All links must have quditto_rate_r0 > 0 (${invalidQudittoRateR0.length} invalid).`);
  }

  const invalidQudittoRateAlpha = links.filter(
    (link) =>
      link.linkType !== "PQC" &&
      (!Number.isFinite(Number(link.qudittoRateAlpha)) || Number(link.qudittoRateAlpha) < 0)
  );
  if (invalidQudittoRateAlpha.length > 0) {
    errors.push(`All links must have quditto_rate_alpha >= 0 (${invalidQudittoRateAlpha.length} invalid).`);
  }

  if (nodes.length > 1) {
    const adjacency = new Map<string, Set<string>>();
    for (const node of nodes) {
      adjacency.set(node.uid, new Set());
    }

    for (const link of links) {
      if (!adjacency.has(link.sourceUid) || !adjacency.has(link.targetUid)) continue;
      if (link.sourceUid === link.targetUid) continue;
      adjacency.get(link.sourceUid)?.add(link.targetUid);
      adjacency.get(link.targetUid)?.add(link.sourceUid);
    }

    const firstNode = nodes[0]?.uid;
    const visited = new Set<string>();

    if (firstNode) {
      const queue: string[] = [firstNode];
      while (queue.length > 0) {
        const current = queue.shift();
        if (!current || visited.has(current)) continue;
        visited.add(current);
        const nextNodes = adjacency.get(current);
        if (!nextNodes) continue;
        for (const next of nextNodes) {
          if (!visited.has(next)) {
            queue.push(next);
          }
        }
      }
    }

    if (visited.size !== nodes.length) {
      warnings.push(`Network is not fully connected: ${visited.size}/${nodes.length} nodes reachable.`);
    }
  }

  return { errors, warnings };
}
