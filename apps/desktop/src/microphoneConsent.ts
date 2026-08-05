const WINDOWS_MICROPHONE_NOTICE_KEY = "lumen.windows.microphone-notice.v1";

type ConsentStorage = Pick<Storage, "getItem" | "setItem">;

export function hasAcknowledgedWindowsMicrophoneNotice(storage: ConsentStorage): boolean {
  try {
    return storage.getItem(WINDOWS_MICROPHONE_NOTICE_KEY) === "acknowledged";
  } catch {
    return false;
  }
}

export function acknowledgeWindowsMicrophoneNotice(storage: ConsentStorage): void {
  try {
    storage.setItem(WINDOWS_MICROPHONE_NOTICE_KEY, "acknowledged");
  } catch {
    // Storage can be unavailable in hardened environments. The caller keeps
    // an in-memory acknowledgement so the current recording can still start.
  }
}
