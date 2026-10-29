// Injected at build/dev start from root CHANGELOG.md (## X.Y.Z).
declare const __APEX_VERSION__: string;
export const VERSION: string =
  typeof __APEX_VERSION__ === 'string' ? __APEX_VERSION__ : '0.0.0';
