import { execFileSync } from "node:child_process";
for (const framework of ["svelte", "react"])
  for (const chart of ["uplot", "echarts"])
    execFileSync(process.execPath, ["node_modules/vite/bin/vite.js", "build"], {
      stdio: "inherit",
      env: { ...process.env, CVTL_FRAMEWORK: framework, CVTL_CHART: chart },
    });
