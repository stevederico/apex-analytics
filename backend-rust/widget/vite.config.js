import { defineConfig } from 'vite';

export default defineConfig({
  build: {
    lib: {
      entry: 'src/index.js',
      name: 'DottieFeedback',
      formats: ['iife'],
      fileName: () => 'feedback.js',
    },
    outDir: 'dist',
    emptyOutDir: true,
    minify: true,
    cssCodeSplit: false,
  },
});
