// Сценарий использует рабочие React-маршруты и моки REST из оригинала.
import { _electron, expect, type Page } from "@playwright/test";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import { WorkspaceApiMock } from "../e2e/helpers/workspace-api-mock";
import { expectReadyMessageComposer } from "../e2e/helpers/message-composer";
import { e2eOrgBasePath } from "../e2e/helpers/navigate-messenger";
import { createMessengerPlacementUuid } from "../packages/web/src/entities/messenger/messenger-message-identity.lib";
import {
  E2E_ACCOUNT_ID,
  E2E_INSTANCE_ID,
  E2E_ORGANIZATION_ID,
  E2E_PROJECT_ID,
  E2E_USER_UUID,
  E2E_STREAM_UUID,
  E2E_TOPIC_UUID,
  messageSuccess,
  streamsSuccess,
  topicsSuccess,
  usersSuccess,
} from "../e2e/mocks/workspace-default-responses";
import type fixture from "./scenes.json";

type Scene = (typeof fixture.scenes)[number];
type Settings = {
  origin: string;
  electron: { entry: string; executable: string };
  output: string;
  profile: string;
  sceneConfig: typeof fixture;
};

const peer = "22222222-2222-4222-8222-222222222223";
const direct = "33333333-3333-4333-8333-333333333334";
const directTopic = "44444444-4444-4444-8444-444444444445";
const emptyTopic = "44444444-4444-4444-8444-444444444446";
const datePrefix = "2026-07-16T10:";
const bubbles = "[data-workspace-message-bubble]";
const loadingIndicator =
  '[aria-label="Загрузка сообщений"][aria-busy="true"] span.animate-spin';
const timeout = 15_000;

function gate() {
  let release!: () => void;
  const promise = new Promise<void>((resolve) => {
    release = resolve;
  });
  return { promise, release };
}

function atMinute(minute: number) {
  return `${datePrefix}${String(minute).padStart(2, "0")}:00.000Z`;
}

function makeData(config: typeof fixture) {
  const users = config.profiles.map((profile) => ({
    ...usersSuccess()[0],
    uuid: profile.id === "a" ? E2E_USER_UUID : peer,
    username: `ui-parity-${profile.id}`,
    first_name: profile.firstName,
    last_name: profile.lastName,
    email: `${profile.id}@example.test`,
    avatar: profile.avatar,
  }));
  const conversations = [
    { stream: direct, topic: directTopic, ...config.direct },
    {
      stream: E2E_STREAM_UUID,
      topic: E2E_TOPIC_UUID,
      ...config.channel.topics[0],
    },
    { stream: E2E_STREAM_UUID, topic: emptyTopic, ...config.channel.topics[1] },
  ];
  const messages = conversations.flatMap((chat) =>
    chat.messages.map((msg) => ({
      ...messageSuccess(
        msg.text,
        `55555555-5555-4555-8555-${String(msg.minute).padStart(12, "0")}`,
      ),
      stream_uuid: chat.stream,
      topic_uuid: chat.topic,
      author_uuid: msg.author === "a" ? E2E_USER_UUID : peer,
      is_own: msg.author === "a",
      created_at: atMinute(msg.minute),
      updated_at: atMinute(msg.minute),
    })),
  );
  const tail = (id: string) =>
    messages.findLast((message) => message.topic_uuid === id)?.uuid ?? null;
  const unread = config.channel.topics[0].unread;
  const streams = [
    {
      ...streamsSuccess()[0],
      uuid: direct,
      name: config.direct.title,
      private: true,
      direct_user_uuid: peer,
      last_message_uuid: tail(directTopic),
    },
    {
      ...streamsSuccess()[0],
      name: config.channel.title,
      unread_count: unread,
      active_unread_count: unread,
      last_message_uuid: tail(E2E_TOPIC_UUID),
    },
  ];
  const topics = conversations.map((chat) => ({
    ...topicsSuccess()[0],
    uuid: chat.topic,
    stream_uuid: chat.stream,
    name: chat.title,
    is_default: chat.topic !== emptyTopic,
    unread_count: chat.topic === E2E_TOPIC_UUID ? unread : 0,
    active_unread_count: chat.topic === E2E_TOPIC_UUID ? unread : 0,
    last_message_uuid: tail(chat.topic),
  }));
  const base = e2eOrgBasePath();
  const paths: Record<string, string> = {
    direct: `${base}/stream/${direct}`,
    "direct-topic": `${base}/stream/${direct}/topic/${directTopic}`,
    topic: `${base}/stream/${E2E_STREAM_UUID}/topic/${E2E_TOPIC_UUID}`,
    "empty-topic": `${base}/stream/${E2E_STREAM_UUID}/topic/${emptyTopic}`,
  };
  return { users, streams, topics, messages, paths };
}

async function seedSession(
  page: Page,
  settings: Settings,
  users: ReturnType<typeof makeData>["users"],
) {
  const { sceneConfig, origin } = settings;
  const sessions = users.map((user, index) => ({
    accountId: index === 0 ? E2E_ACCOUNT_ID : "e2e-account-b",
    instanceId: E2E_INSTANCE_ID,
    organizationId: E2E_ORGANIZATION_ID,
    organizationOrigin: origin,
    projectId: E2E_PROJECT_ID,
    userUuid: user.uuid,
    login: user.email,
    accessToken: "local-fixture-no-server-credentials",
    refreshToken: "local-fixture-no-server-credentials",
    expiresAtMs: Date.parse(sceneConfig.clock) + 3_600_000,
    runtimeGeneration: 0,
    profile: {
      uuid: user.uuid,
      username: user.username,
      firstName: user.first_name,
      lastName: user.last_name,
      email: user.email,
      status: "active",
    },
  }));
  const ownerKeys = sessions.map((session) =>
    [
      "account",
      session.accountId,
      "instance",
      session.instanceId,
      "organization",
      session.organizationId,
      "project",
      session.projectId,
      "user",
      session.userUuid,
    ].join(":"),
  );
  await page.addInitScript(
    ({ sessions, ownerKeys, origin, language, palette }) => {
      if (location.origin !== origin) return;
      if (localStorage.getItem("workspace-auth-sessions") === null) {
        localStorage.setItem(
          "workspace-auth-sessions",
          JSON.stringify(sessions),
        );
        localStorage.setItem(
          "workspace-auth-current-account",
          sessions[0].accountId,
        );
        for (const key of ownerKeys)
          localStorage.setItem(`workspace-theme-mode:${key}`, "light");
      }
      localStorage.setItem("workspace-locale", language);
      for (const key of ownerKeys) {
        localStorage.setItem(`workspace-locale:${key}`, language);
        localStorage.setItem(`workspace-palette:${key}`, palette);
      }
    },
    {
      sessions,
      ownerKeys,
      origin,
      language: sceneConfig.language,
      palette: sceneConfig.palette,
    },
  );
  return ownerKeys;
}

async function installMock(
  page: Page,
  origin: string,
  data: ReturnType<typeof makeData>,
) {
  // Первый маршрут вызывается последним и оставляет доступ только к локальному Vite.
  await page.route("**/*", (route) => {
    const url = new URL(route.request().url());
    const isApi = /\/(?:api\/workspace|workspace)\/v1\//.test(url.pathname);
    return url.origin === origin && !isApi ? route.continue() : route.abort();
  });
  await new WorkspaceApiMock(page).install();
  await page.routeWebSocket("**/*", (socket) => {
    const url = new URL(socket.url());
    if (
      url.host === new URL(origin).host &&
      url.pathname.endsWith("/events/ws")
    ) {
      socket.send(
        JSON.stringify({
          type: "ready",
          epoch_generation: "e2e-generation-1",
          epoch_version: 1,
        }),
      );
    } else {
      socket.close();
    }
  });

  let emptyState: "loading" | "error" | "empty" = "loading";
  let historyRequests = 0;
  let sendCount = 0;
  const load = gate();
  const send = gate();
  const committed: ReturnType<typeof messageSuccess>[] = [];
  const api =
    /\/(?:api\/workspace|workspace)\/v1\/(?:messenger\/)?(streams|stream_topics|users|messages)(?:\/([^/?]+))?\/?(?:\?.*)?$/;
  await page.route(api, async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const [, resource, id] = api.exec(url.pathname) ?? [];
    const q = url.searchParams;
    if (resource === "messages" && !id && request.method() === "POST") {
      const body = request.postDataJSON();
      if (
        body.stream_uuid !== direct ||
        body.topic_uuid !== directTopic ||
        !body.uuid
      ) {
        throw new Error("Запрос отправки не привязан к синтетической личке");
      }
      if (++sendCount === 1) {
        await send.promise;
        await route.fulfill({
          status: 503,
          json: { message: "Синтетический сбой доставки" },
        });
      } else {
        const item = {
          ...messageSuccess(
            body.payload.content,
            createMessengerPlacementUuid(directTopic, body.uuid),
          ),
          stream_uuid: direct,
          topic_uuid: directTopic,
          created_at: atMinute(20),
          updated_at: atMinute(20),
        };
        committed.push(item);
        await route.fulfill({ status: 201, json: item });
      }
      return;
    }
    if (request.method() !== "GET") return route.fallback();
    if (
      resource === "users" ||
      resource === "streams" ||
      resource === "stream_topics"
    ) {
      const items =
        resource === "users"
          ? data.users
          : resource === "streams"
            ? data.streams
            : data.topics.filter(
                (topic) =>
                  !q.has("stream_uuid") ||
                  q.get("stream_uuid") === topic.stream_uuid,
              );
      await route.fulfill({
        json: id ? (items.find((item) => item.uuid === id) ?? null) : items,
      });
      return;
    }
    if (resource !== "messages") return route.fallback();
    if (q.get("mentioned") === "true") {
      await route.fulfill({ json: [] });
      return;
    }
    if (q.get("topic_uuid") === emptyTopic) {
      historyRequests += 1;
      if (emptyState === "loading") await load.promise;
      if (emptyState === "error") {
        await route.fulfill({
          status: 503,
          json: { message: "Синтетический сбой истории" },
        });
        return;
      }
    }
    const ids = q.getAll("uuid");
    const items = [...data.messages, ...committed].filter(
      (item) =>
        (!q.has("stream_uuid") || q.get("stream_uuid") === item.stream_uuid) &&
        (!q.has("topic_uuid") || q.get("topic_uuid") === item.topic_uuid) &&
        (!q.has("read") || q.get("read") === String(item.read)) &&
        (!ids.length || ids.includes(item.uuid)),
    );
    if (q.get("sort_dir") === "desc") items.reverse();
    const mark = items.findIndex((item) => item.uuid === q.get("page_marker"));
    const pageLimit = Number(q.get("page_limit")) || items.length;
    await route.fulfill({
      json: id
        ? (items.find((item) => item.uuid === id) ?? null)
        : items.slice(mark + 1, mark + 1 + pageLimit),
    });
  });
  return {
    releaseSend: send.release,
    historyRequests: () => historyRequests,
    failHistory() {
      emptyState = "error";
      load.release();
    },
    emptyHistory() {
      emptyState = "empty";
    },
    sendCount: () => sendCount,
  };
}

// Оригинал скрывает первичный индикатор, как только IndexedDB вернул даже пустое окно.
// Удерживаем ответ чтения для одной темы, чтобы снять именно этот кадр.
async function holdInitialCacheRead(page: Page) {
  return page.evaluateHandle((topicUuid) => {
    const originalGet = IDBObjectStore.prototype.get;
    const success = Object.getOwnPropertyDescriptor(
      IDBRequest.prototype,
      "onsuccess",
    )!;
    let finish!: () => void;
    let reads = 0;
    const ready = new Promise<void>((resolve) => {
      finish = resolve;
    });
    IDBObjectStore.prototype.get = function (key) {
      const request = originalGet.call(this, key);
      if (
        this.name === "messageWindows" &&
        this.transaction.mode === "readonly" &&
        typeof key === "string" &&
        key.endsWith(`:${topicUuid}`)
      ) {
        reads += 1;
        Object.defineProperty(request, "onsuccess", {
          set(listener) {
            success.set!.call(request, (event: Event) => {
              void ready.then(() => listener.call(request, event));
            });
          },
        });
      }
      return request;
    };
    return {
      reads: () => reads,
      release() {
        IDBObjectStore.prototype.get = originalGet;
        finish();
      },
    };
  }, emptyTopic);
}

async function assertScene(page: Page, scene: Scene, config: typeof fixture) {
  const outgoing = page
    .locator(bubbles)
    .filter({ hasText: config.direct.outgoing });
  switch (scene.state) {
    case "ready": {
      const chat =
        scene.route === "direct" ? config.direct : config.channel.topics[0];
      await expect(page.locator(bubbles)).toHaveCount(chat.messages.length, {
        timeout,
      });
      await expect(
        page.getByText(chat.messages[0].text, { exact: true }),
      ).toBeVisible({ timeout });
      if (scene.route === "direct") {
        await expect(
          page.getByRole("navigation", { name: "Выберите тему:" }),
        ).toBeVisible({ timeout });
      } else {
        await expectReadyMessageComposer(page);
      }
      break;
    }
    case "sending":
      await expect(outgoing).toHaveAttribute(
        "data-outgoing-delivery-status",
        "sending",
        { timeout },
      );
      break;
    case "failed":
      await expect(outgoing).toHaveAttribute(
        "data-outgoing-delivery-status",
        "failed",
        { timeout },
      );
      break;
    case "confirmed":
      await expect(outgoing).toHaveCount(1, { timeout });
      await expect(outgoing).not.toHaveAttribute(
        "data-outgoing-delivery-status",
        /.+/,
        { timeout },
      );
      break;
    case "error":
      await expect(
        page.getByText("Не удалось загрузить сообщения", { exact: true }),
      ).toBeVisible({ timeout });
      await expect(
        page.getByRole("button", { name: "Повторить", exact: true }),
      ).toBeVisible({ timeout });
      break;
    case "loading":
      await expect(page.locator(loadingIndicator)).toBeVisible({ timeout });
      break;
    case "empty":
      await expectReadyMessageComposer(page);
      await expect(page.locator('[data-empty-state="true"]')).toBeVisible({
        timeout,
      });
      await expect(
        page.locator('[aria-label="Загрузка сообщений"][aria-busy="true"]'),
      ).toHaveCount(0, { timeout });
      await expect(page.locator(bubbles)).toHaveCount(0, { timeout });
      break;
    default:
      throw new Error(`Неизвестное состояние ${scene.state}`);
  }
}

async function prepareCapture(page: Page) {
  return page.evaluate(async () => {
    // Не ждём бесконечные CSS-анимации и фиксируем первый кадр индикатора.
    await document.fonts.ready;
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(
      "*, *::before, *::after { animation-play-state: paused !important; transition: none !important; caret-color: transparent !important; }",
    );
    document.adoptedStyleSheets = [sheet];
    for (const animation of document.getAnimations()) {
      animation.pause();
      await animation.ready;
      animation.currentTime = 0;
    }
    if (document.activeElement instanceof HTMLElement)
      document.activeElement.blur();
    await new Promise<void>((resolve) =>
      requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
    );
    return {
      width: innerWidth,
      height: innerHeight,
      scale: devicePixelRatio,
      language: document.documentElement.lang,
      timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      font: getComputedStyle(document.body).fontFamily,
      palette: document.documentElement.dataset.palette,
      theme: document.documentElement.dataset.theme,
    };
  });
}

export async function capture(settings: Settings) {
  const { origin, electron, profile, output, sceneConfig } = settings;
  const electronApp = await _electron.launch({
    executablePath: electron.executable,
    args: [
      electron.entry,
      "--force-device-scale-factor=1",
      "--lang=ru",
      "--use-mock-keychain",
    ],
    env: {
      ...process.env,
      WORKSPACE_UI_PARITY_PROFILE: profile,
      TZ: sceneConfig.timezone,
    },
    chromiumSandbox: true,
  });
  try {
    const page = await electronApp.firstWindow();
    page.setDefaultTimeout(timeout);
    const nativeWindow = await electronApp.browserWindow(page);
    const data = makeData(sceneConfig);
    await page.clock.setFixedTime(new Date(sceneConfig.clock));
    const ownerKeys = await seedSession(page, settings, data.users);
    const mock = await installMock(page, origin, data);
    const runtime = await electronApp.evaluate(() => process.versions.electron);
    let last: Scene | undefined;
    let firstWindowId: number | undefined;
    let initialCacheRead:
      | Awaited<ReturnType<typeof holdInitialCacheRead>>
      | undefined;
    const failures: Error[] = [];
    for (const scene of sceneConfig.scenes) {
      console.log(`Сцена ${scene.id}`);
      try {
        await nativeWindow.evaluate(
          (window, { width, height }) => window.setContentSize(width, height),
          scene,
        );
        if (scene.state === "loading")
          initialCacheRead = await holdInitialCacheRead(page);
        if (scene.route !== last?.route || scene.theme !== last.theme) {
          if (last)
            await page.evaluate(
              ({ ownerKeys, theme }) => {
                for (const key of ownerKeys)
                  localStorage.setItem(`workspace-theme-mode:${key}`, theme);
              },
              { ownerKeys, theme: scene.theme },
            );
          const url = `${origin}/#${data.paths[scene.route]}`;
          if (last?.route !== scene.route) await page.goto(url);
          if (!last || last.theme !== scene.theme) await page.reload();
        }
        last = scene;
        await expect(page.locator("[data-focus-zone='topbar']")).toBeVisible({
          timeout,
        });
        await expect(page.locator("html")).toHaveAttribute(
          "data-theme",
          scene.theme,
          { timeout },
        );
        await expect(page.locator("html")).toHaveAttribute(
          "data-palette",
          sceneConfig.palette,
          { timeout },
        );
        const dismiss = page.getByRole("button", {
          name: "Не сейчас",
          exact: true,
        });
        if (await dismiss.isVisible()) await dismiss.click();
        await expect(
          page.getByText("Включить уведомления на рабочем столе?", {
            exact: true,
          }),
        ).toHaveCount(0);
        if (scene.state === "sending") {
          const textbox = await expectReadyMessageComposer(page);
          await textbox.fill(sceneConfig.direct.outgoing);
          await textbox.press("Enter");
          await expect.poll(mock.sendCount, { timeout }).toBe(1);
        } else if (scene.state === "failed") {
          mock.releaseSend();
        } else if (scene.state === "confirmed") {
          await page
            .getByRole("button", { name: /^(повторить отправку|retry send)$/i })
            .click();
          await expect.poll(mock.sendCount, { timeout }).toBe(2);
        } else if (scene.state === "loading") {
          await expect
            .poll(() => initialCacheRead!.evaluate((gate) => gate.reads()), {
              timeout,
            })
            .toBeGreaterThan(0);
        } else if (scene.state === "error") {
          await initialCacheRead?.evaluate((gate) => gate.release());
          await initialCacheRead?.dispose();
          await expect
            .poll(mock.historyRequests, { timeout })
            .toBeGreaterThan(0);
          mock.failHistory();
        } else if (scene.state === "empty") {
          mock.emptyHistory();
          await page
            .getByRole("button", { name: "Повторить", exact: true })
            .click();
        }
        await assertScene(page, scene, sceneConfig);
        const renderer = await prepareCapture(page);
        expect([
          renderer.width,
          renderer.height,
          renderer.scale,
          renderer.language,
          renderer.timezone,
        ]).toEqual([
          scene.width,
          scene.height,
          sceneConfig.deviceScaleFactor,
          sceneConfig.language,
          sceneConfig.timezone,
        ]);
        let spinner = null;
        if (scene.state === "loading") {
          await assertScene(page, scene, sceneConfig);
          spinner = await page.locator(loadingIndicator).evaluate((element) => {
            const { x, y, width, height } = element.getBoundingClientRect();
            return {
              x,
              y,
              width,
              height,
              layoutWidth: element.offsetWidth,
              layoutHeight: element.offsetHeight,
            };
          });
          expect([spinner.layoutWidth, spinner.layoutHeight]).toEqual([32, 32]);
        }
        const basename = join(output, scene.id);
        const { png, ...window } = await nativeWindow.evaluate(
          async (window) => {
            const frame = await window.capturePage();
            return {
              bounds: window.getContentBounds(),
              image: frame.getSize(),
              trafficLights: window.getWindowButtonPosition(),
              windowId: window.id,
              visible: window.isVisible(),
              png: frame.toPNG().toString("base64"),
            };
          },
        );
        firstWindowId ??= window.windowId;
        expect(electronApp.windows()).toHaveLength(1);
        expect(window.windowId).toBe(firstWindowId);
        expect(window.visible).toBe(true);
        expect([window.image.width, window.image.height]).toEqual([
          scene.width,
          scene.height,
        ]);
        if (scene.state === "loading")
          await assertScene(page, scene, sceneConfig);
        await writeFile(`${basename}.png`, Buffer.from(png, "base64"));
        await writeFile(
          `${basename}.json`,
          `${JSON.stringify(
            {
              ...scene,
              commit: sceneConfig.commit,
              clock: sceneConfig.clock,
              source: "electron-browser-window-content",
              runtime,
              renderer,
              window,
              ...(spinner === null ? {} : { spinner }),
            },
            null,
            2,
          )}\n`,
        );
      } catch (error) {
        const failure = new Error(
          `Сцена ${scene.id}: ожидаемое состояние не подтверждено`,
          { cause: error },
        );
        console.error(failure);
        failures.push(failure);
      }
    }
    if (failures.length) throw new Error(`Не снято кадров: ${failures.length}`);
  } finally {
    await electronApp.close();
  }
}
