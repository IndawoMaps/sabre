import { requireOptionalNativeModule } from "expo";

/** What the native side reports once the server is listening. */
export interface NativeEndpoint {
  port: number;
  token: string;
  baseUrl: string;
  fileRoot: string;
}

interface SabreNativeModule {
  start(fileRoot: string | null, cacheBytes: number): Promise<NativeEndpoint>;
  stop(): void;
  listRasters(fileRoot: string | null): string[];
}

let loaded: SabreNativeModule | null | undefined;

/** The native module, or a clear error for why it is missing. */
export function native(): SabreNativeModule {
  if (loaded === undefined) {
    loaded = requireOptionalNativeModule<SabreNativeModule>("Sabre");
  }
  if (!loaded) {
    throw new Error(
      "@sabremaps/react-native: the native module is not in this build. After installing " +
      "it, rebuild the app (`expo run:android`, `expo run:ios` or a new dev client) -- " +
      "Expo Go cannot load it.",
    );
  }
  return loaded;
}
