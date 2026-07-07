import { defineConfig, presetUno } from 'unocss'

export default defineConfig({
  presets: [presetUno()],
  content: {
    pipeline: {
      include: [/\.([jt]sx?|html)($|\?)/],
    },
    filesystem: ['index.html', 'src/**/*.{ts,tsx,js,jsx}'],
  },
})
