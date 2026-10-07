import {defineConfig} from 'vite';
import react from '@vitejs/plugin-react';

const backend = process.env.TAVILY_LB_BACKEND ?? 'http://127.0.0.1:8080';

export default defineConfig({
  base: '/admin/',
  plugins: [react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    chunkSizeWarningLimit: 2048,
  },
  server: {
    proxy: {
      '/api': backend,
    },
  },
});
