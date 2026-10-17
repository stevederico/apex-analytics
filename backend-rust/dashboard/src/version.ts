// Injected at build/dev start from root CHANGELOG.md (## X.Y.Z).
declare const __DOTTIE_VERSION__: string;
export const VERSION: string =
  typeof __DOTTIE_VERSION__ === 'string' ? __DOTTIE_VERSION__ : '0.0.0';
