import type { Rect, Target } from "./api";

export const MIN_REGION = 64;

export function defaultRegion(target: Target): Rect {
  const width = Math.min(target.width, Math.max(MIN_REGION, Math.round(target.width * 0.68)));
  const height = Math.min(target.height, Math.max(MIN_REGION, Math.round(target.height * 0.68)));
  return {
    x: Math.floor((target.width - width) / 2),
    y: Math.floor((target.height - height) / 2),
    width,
    height,
  };
}

export function validRegion(region: Rect, target: Target) {
  return Object.values(region).every(Number.isSafeInteger) &&
    region.x >= 0 && region.y >= 0 &&
    region.width >= MIN_REGION && region.height >= MIN_REGION &&
    region.x + region.width <= target.width && region.y + region.height <= target.height;
}

export function pixels(rect: Rect) {
  return rect.width + " × " + rect.height;
}
