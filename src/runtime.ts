interface RuntimeCrypto {
  randomUUID?: () => string;
  getRandomValues?: (array: Uint8Array) => Uint8Array;
}

interface RuntimeNavigator {
  platform?: string;
  userAgent?: string;
}

export function createClientId(cryptoSource: RuntimeCrypto | undefined = globalThis.crypto): string {
  if (cryptoSource?.randomUUID) return cryptoSource.randomUUID();

  const bytes = new Uint8Array(16);
  if (cryptoSource?.getRandomValues) {
    cryptoSource.getRandomValues(bytes);
  } else {
    fillWithMathRandom(bytes);
  }

  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;

  return [
    toHex(bytes.subarray(0, 4)),
    toHex(bytes.subarray(4, 6)),
    toHex(bytes.subarray(6, 8)),
    toHex(bytes.subarray(8, 10)),
    toHex(bytes.subarray(10, 16))
  ].join("-");
}

export function getDeviceName(
  configuredDeviceName: string | undefined,
  navigatorSource: RuntimeNavigator | undefined = globalThis.navigator
): string {
  const configured = configuredDeviceName?.trim();
  if (configured) return configured;

  const platform = navigatorSource?.platform?.trim();
  if (platform) return platform;

  const userAgent = navigatorSource?.userAgent?.trim();
  if (userAgent) return userAgent;

  return "Obsidian device";
}

function toHex(bytes: Uint8Array): string {
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function fillWithMathRandom(bytes: Uint8Array): void {
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Math.floor(Math.random() * 256);
  }
}
