import type { AudioDevice } from "./types";

export function chooseAudioDevice(
  devices: AudioDevice[],
  preferred: string | null | undefined,
): string {
  const saved = preferred?.trim();
  if (saved && devices.some((device) => device.name === saved)) {
    return saved;
  }

  return (devices.find((device) => device.is_default) ?? devices[0])?.name ?? "";
}
