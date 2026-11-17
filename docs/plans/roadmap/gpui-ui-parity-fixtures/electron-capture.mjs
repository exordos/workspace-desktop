// Запускает настоящий main/preload Electron с изолированным профилем и локальной сборкой React.
import { build } from "esbuild";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const stage = dirname(fileURLToPath(import.meta.url));
const root = dirname(stage);
const profile = await mkdtemp(join(tmpdir(), "workspace-ui-02-profile-"));
const sceneConfig = JSON.parse(
  await readFile(join(stage, "scenes.json"), "utf8"),
);
const nodeRequire = createRequire(import.meta.url);
let server;
let electronStage;

// Подменяем только адрес dev-сервера, каталог профиля, предел размера окна и вызов отдельного окна DevTools.
async function buildElectron(origin) {
  const packageRoot = join(root, "packages/electron");
  const source = join(packageRoot, "src");
  electronStage = await mkdtemp(join(packageRoot, ".ui-parity-"));
  let main = await readFile(join(source, "main.ts"), "utf8");
  const replacements = [
    [
      'const DEV_SERVER_URL = "http://localhost:5173";',
      `const DEV_SERVER_URL = ${JSON.stringify(origin)};`,
    ],
    [
      "applyUserDataDirectory();",
      `
      app.setPath("userData", process.env.WORKSPACE_UI_PARITY_PROFILE!);
      app.whenReady().then(() => session.defaultSession.webRequest.onBeforeRequest((details, callback) => {
        const url = new URL(details.url);
        callback({ cancel: url.host !== new URL(DEV_SERVER_URL).host && !["data:", "blob:"].includes(url.protocol) });
      }));
    `,
    ],
    [
      "    minWidth: 700,",
      "    enableLargerThanScreen: true,\n    minWidth: 700,",
    ],
    ['    mainWindow.webContents.openDevTools({ mode: "detach" });', ""],
  ];
  for (const [from, to] of replacements) {
    if (!main.includes(from))
      throw new Error(`Не найден ожидаемый фрагмент Electron main: ${from}`);
    main = main.replace(from, to);
  }
  const common = {
    bundle: true,
    platform: "node",
    format: "cjs",
    external: ["electron"],
  };
  await Promise.all([
    build({
      ...common,
      stdin: {
        contents: main,
        sourcefile: "main.ts",
        resolveDir: source,
        loader: "ts",
      },
      outfile: join(electronStage, "main.js"),
      external: ["electron", "@jitsi/electron-sdk/main"],
      define: { __ELECTRON_DISABLE_AUTO_UPDATE__: "true" },
    }),
    build({
      ...common,
      entryPoints: [join(source, "preload.ts")],
      outfile: join(electronStage, "preload.js"),
    }),
  ]);
  return {
    entry: join(electronStage, "main.js"),
    executable: createRequire(join(packageRoot, "package.json"))("electron"),
  };
}

try {
  // Vite установлен в web workspace и может не быть поднят npm к корню монорепозитория.
  const webRequire = createRequire(join(root, "packages/web/package.json"));
  const { build: buildWeb, preview } = await import(webRequire.resolve("vite"));
  const webConfig = {
    root: join(root, "packages/web"),
    base: "/",
    logLevel: "warn",
    build: { outDir: join(stage, "web") },
    preview: { host: "127.0.0.1", port: 43170, strictPort: true },
  };
  process.env.ELECTRON = "1";
  await buildWeb(webConfig);
  server = await preview(webConfig);
  const origin = "http://127.0.0.1:43170";
  const scenarioPath = join(stage, "electron-scenario.cjs");
  await build({
    entryPoints: [join(stage, "electron-scenario.ts")],
    outfile: scenarioPath,
    bundle: true,
    platform: "node",
    format: "cjs",
    packages: "external",
  });
  const { capture } = nodeRequire(scenarioPath);
  const electron = await buildElectron(origin);
  await capture({
    electron,
    origin,
    profile,
    output: process.argv[2],
    sceneConfig,
  });
} finally {
  if (server)
    await new Promise((resolve, reject) =>
      server.httpServer.close((error) => (error ? reject(error) : resolve())),
    );
  await rm(profile, { recursive: true, force: true });
  if (electronStage) await rm(electronStage, { recursive: true, force: true });
}
