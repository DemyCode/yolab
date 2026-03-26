/** @type {import('tailwindcss').Config} */
export default {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        bg: '#0d0d0d',
        surface: '#141414',
        border: '#222222',
        accent: '#6ee7b7',
        muted: '#6b7280',
      },
    },
  },
}
