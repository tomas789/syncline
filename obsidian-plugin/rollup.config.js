import resolve from '@rollup/plugin-node-resolve';
import commonjs from '@rollup/plugin-commonjs';
import typescript from '@rollup/plugin-typescript';
import { base64 } from 'rollup-plugin-base64';

export default {
  input: 'main.ts',
  output: {
    file: 'main.js',
    sourcemap: 'inline',
    format: 'cjs',
    exports: 'default',
  },
  external: ['obsidian', 'obsidian/dataview', 'obsidian-calendar-ui'],
  plugins: [
    resolve({ browser: true }),
    commonjs(),
    typescript(),
    base64({ include: '**/*.wasm' })
  ]
};