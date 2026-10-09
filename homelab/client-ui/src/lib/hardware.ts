import type { NodeHardware } from "@/types/nodes";

const ARCH_LABELS: Record<string, string> = {
  amd64: "x86",
  arm64: "ARM",
};

const ACCELERATOR_LABELS: Record<string, string> = {
  nvidia: "NVIDIA GPU",
  "nvidia-legacy": "Older NVIDIA GPU · games only",
  amd: "AMD GPU",
  intel: "Intel GPU",
  cpu: "CPU only",
};

export function hardwareLabel(h: NodeHardware | undefined): string | null {
  if (!h?.accelerator) return null;
  if (h.accelerator === "cpu" && h.video_gpu) return "GPU · video only";
  return ACCELERATOR_LABELS[h.accelerator] ?? h.accelerator;
}

export function hardwareDetail(h: NodeHardware | undefined): string | null {
  if (!h) return null;
  const parts = [
    h.arch ? (ARCH_LABELS[h.arch] ?? h.arch) : null,
    h.vram_gib ? `${h.vram_gib} GB VRAM` : null,
    h.ram_gib ? `${h.ram_gib} GB RAM` : null,
  ].filter((p): p is string => p !== null);
  return parts.length > 0 ? parts.join(" · ") : null;
}
