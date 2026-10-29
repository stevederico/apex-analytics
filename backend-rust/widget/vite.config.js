import { defineConfig } from 'vite';

export default defineConfig({
  build: {
    lib: {
      entry: 'src/index.js',
      name: 'ApexFeedback',
      formats: ['iife'],
      fileName: () => 'feedback.js',
    },
    outDir: 'dist',
    emptyOutDir: true,
    minify: true,
    cssCodeSplit: false,
  },
});
