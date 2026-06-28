interface RuntimeCrypto {
  randomUUID?: () => string;
  getRandomValues?: (array: Uint8Array) => Uint8Array;
}

interface RuntimeNavigator {
  platform?: string;
  userAgent?: string;
}

const DEVICE_NAME_ANIMALS = [
  "Badger",
  "Falcon",
  "Fox",
  "Hawk",
  "Lynx",
  "Otter",
  "Panda",
  "Raven",
  "Seal",
  "Wolf"
];

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

export function generateComputerName(
  cryptoSource: RuntimeCrypto | undefined = globalThis.crypto,
  navigatorSource: RuntimeNavigator | undefined = globalThis.navigator
): string {
  const prefix = detectDevicePrefix(navigatorSource);
  const animal = DEVICE_NAME_ANIMALS[randomIndex(DEVICE_NAME_ANIMALS.length, cryptoSource)];
  return `${prefix}-${animal}`;
}

export function slugFromName(name: string | undefined, fallback: string): string {
  const slug = (name ?? "")
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 96);
  return slug || fallback;
}

function detectDevicePrefix(navigatorSource: RuntimeNavigator | undefined): "Mac" | "iPhone" {
  const platform = navigatorSource?.platform?.toLowerCase() ?? "";
  const userAgent = navigatorSource?.userAgent?.toLowerCase() ?? "";
  if (platform.includes("iphone") || userAgent.includes("iphone")) return "iPhone";
  return "Mac";
}

function randomIndex(length: number, cryptoSource: RuntimeCrypto | undefined): number {
  const bytes = new Uint8Array(1);
  if (cryptoSource?.getRandomValues) {
    cryptoSource.getRandomValues(bytes);
  } else {
    fillWithMathRandom(bytes);
  }
  return bytes[0] % length;
}

function toHex(bytes: Uint8Array): string {
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function fillWithMathRandom(bytes: Uint8Array): void {
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Math.floor(Math.random() * 256);
  }
}
