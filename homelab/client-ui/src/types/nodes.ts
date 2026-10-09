export interface NodeInfo {
  name: string;
  ip: string;
  ready: boolean;
  roles: string[];
  joined_at: string;
  hardware?: NodeHardware;
}

export interface NodeHardware {
  arch?: string | null;
  accelerator:
    "nvidia" | "nvidia-legacy" | "amd" | "vulkan" | "intel" | "cpu" | null;
  video_gpu?: boolean;
  vram_gib: number | null;
  ram_gib: number | null;
  game_input: boolean;
}

export interface NodeLink {
  name: string;
  url: string;
}
