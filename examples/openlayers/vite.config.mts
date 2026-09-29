import { defineConfig } from 'vite'

export default defineConfig({
  build: {
    target: 'es2022',
    rollupOptions: {
      input: { main: 'index.html', sabre: 'sabre.html' },
    },
  },
  server: {
    port: 4200,
    proxy: {
      '/tiles':     'http://localhost:8787',
      '/info':      'http://localhost:8787',
      '/tile-info': 'http://localhost:8787',
    }
  }
})
