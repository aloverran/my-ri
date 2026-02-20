import { defineConfig } from 'vite';
import solid from 'vite-plugin-solid';

export default defineConfig({
  plugins: [solid()],
  server: {
    host: '0.0.0.0', // Allow connections from other devices on the network (including Tailscale)
    allowedHosts: true, // Allow all hosts (alternatively use an array of specific hosts)
    proxy: {
      '/api': {
        target: 'http://localhost:3001',
        changeOrigin: true,
      },
    },
  },
});