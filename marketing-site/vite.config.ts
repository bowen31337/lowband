import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vite";

export default defineConfig({
  // Relative base so the build works both locally and under a
  // project-pages path like https://<user>.github.io/lowband/.
  base: "./",
  plugins: [tailwindcss()],
});
