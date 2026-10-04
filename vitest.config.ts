import { defineConfig } from 'vitest/config';

export default defineConfig({
	test: {
		include: ['{apps,libs}/**/*.test.ts'],
		exclude: ['**/node_modules/**'],
		passWithNoTests: true,
	},
});
