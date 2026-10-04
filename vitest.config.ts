import { defineConfig } from 'vitest/config';

export default defineConfig({
	test: {
		// The suites assert the map, not the run: the sandbox's shift is taken off, so a test reads the
		// pinned numbers wherever it runs.
		env: { LATTICE_PORT_OFFSET: '0' },
		include: ['{apps,libs}/**/*.test.ts'],
		exclude: ['**/node_modules/**'],
		passWithNoTests: true,
	},
});
