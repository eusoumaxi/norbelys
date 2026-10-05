/**
 * The page's motion. Every section is complete without it: the head's inline script adds
 * `.motion` to <html> only when the visitor has not asked for reduced motion, and this module
 * then reveals elements as they arrive and gives each scroll scene its progress.
 *
 * A scene (`data-scene`) receives `--p`, from 0 to 1, eased towards the scroll position so that
 * scrubbed animations glide rather than jump, and `data-stage`, the number of its `data-steps`
 * thresholds already passed. CSS turns those into every transform; nothing here moves a pixel.
 *
 * - `pin`: a tall section with a sticky stage; 0 when its top meets the viewport's top, 1 when
 *   its bottom meets the viewport's bottom.
 * - `enter`: 0 when its top enters at the bottom, 1 when its centre reaches the viewport's centre.
 * - `leave`: 0 while its top is at the viewport's top, 1 once it has scrolled entirely away.
 * - `through` (the default): 0 when its top enters, 1 when its bottom leaves at the top.
 *
 * `data-scene-media` limits a scene to a media query; outside it the scene lets `--p` inherit.
 */

type SceneMode = "pin" | "enter" | "leave" | "through";

interface Scene {
  readonly element: HTMLElement;
  readonly mode: SceneMode;
  readonly steps: readonly number[];
  readonly media: MediaQueryList | undefined;
  current: number;
  target: number;
  stage: number;
}

const clamp = (value: number): number => Math.min(1, Math.max(0, value));

const modeOf = (value: string | undefined): SceneMode => {
  if (value === "pin" || value === "enter" || value === "leave") {
    return value;
  }
  return "through";
};

const progressOf = (scene: Scene, viewport: number): number => {
  const bounds = scene.element.getBoundingClientRect();
  if (scene.mode === "pin") {
    return clamp(-bounds.top / Math.max(1, bounds.height - viewport));
  }
  if (scene.mode === "leave") {
    return clamp(-bounds.top / Math.max(1, bounds.height));
  }
  if (scene.mode === "enter") {
    return clamp(
      (viewport - bounds.top) / Math.max(1, (viewport + bounds.height) / 2)
    );
  }
  return clamp((viewport - bounds.top) / Math.max(1, viewport + bounds.height));
};

const applies = (scene: Scene): boolean => scene.media?.matches ?? true;

/** Writes a scene's progress and stage, only when they change. */
const paint = (scene: Scene): void => {
  scene.element.style.setProperty("--p", scene.current.toFixed(4));
  const stage = scene.steps.filter((step) => scene.current >= step).length;
  if (stage !== scene.stage) {
    scene.stage = stage;
    scene.element.dataset.stage = String(stage);
  }
};

/** Starts the scroll scenes: measured only near the viewport, eased between frames. */
const startScenes = (): void => {
  const scenes: Scene[] = [
    ...document.querySelectorAll<HTMLElement>("[data-scene]"),
  ].map((element) => {
    const query = element.dataset.sceneMedia;
    return {
      current: 0,
      element,
      media: query ? window.matchMedia(query) : undefined,
      mode: modeOf(element.dataset.scene),
      stage: -1,
      steps: (element.dataset.steps ?? "")
        .split(",")
        .filter((step) => step.trim() !== "")
        .map(Number),
      target: 0,
    };
  });
  const near = new Set<Scene>();
  let frame = 0;
  let last = 0;

  const render = (time: number): void => {
    frame = 0;
    const viewport = window.innerHeight;
    // Frame-rate independent easing: the same glide at 60 Hz and at 120 Hz.
    const elapsed = last === 0 ? 16.7 : Math.min(64, time - last);
    last = time;
    const blend = 1 - (1 - 0.16) ** (elapsed / 16.7);
    let moving = false;
    for (const scene of near) {
      if (!applies(scene)) {
        continue;
      }
      scene.target = progressOf(scene, viewport);
      const delta = scene.target - scene.current;
      scene.current =
        Math.abs(delta) < 0.0004 ? scene.target : scene.current + delta * blend;
      if (scene.current !== scene.target) {
        moving = true;
      }
      paint(scene);
    }
    if (moving) {
      frame = requestAnimationFrame(render);
    } else {
      last = 0;
    }
  };
  const request = (): void => {
    if (frame === 0) {
      frame = requestAnimationFrame(render);
    }
  };

  const settle = (scene: Scene): void => {
    if (applies(scene)) {
      scene.target = progressOf(scene, window.innerHeight);
      scene.current = scene.target;
      paint(scene);
    } else {
      scene.element.style.removeProperty("--p");
      delete scene.element.dataset.stage;
      scene.stage = -1;
    }
  };

  const watcher = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        const scene = scenes.find((item) => item.element === entry.target);
        if (!scene) {
          continue;
        }
        if (entry.isIntersecting) {
          near.add(scene);
        } else {
          near.delete(scene);
          settle(scene);
        }
      }
      request();
    },
    { rootMargin: "35% 0px 35% 0px" }
  );
  for (const scene of scenes) {
    settle(scene);
    watcher.observe(scene.element);
    scene.media?.addEventListener("change", () => {
      settle(scene);
      request();
    });
  }
  window.addEventListener("scroll", request, { passive: true });
  window.addEventListener("resize", request, { passive: true });
};

/** Reveals elements once, as they come into view. */
const startReveals = (): void => {
  const revealer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) {
          entry.target.classList.add("is-in");
          revealer.unobserve(entry.target);
        }
      }
    },
    { rootMargin: "0px 0px -8% 0px", threshold: 0.12 }
  );
  for (const element of document.querySelectorAll(
    "[data-reveal], [data-lines], [data-inview]"
  )) {
    revealer.observe(element);
  }
};

/** Runs the motion layer, if the document asked for it. */
export const startMotion = (): void => {
  const root = document.documentElement;
  root.dataset.ready = "";
  if (!root.classList.contains("motion")) {
    for (const element of document.querySelectorAll(
      "[data-reveal], [data-lines], [data-inview]"
    )) {
      element.classList.add("is-in");
    }
    return;
  }
  startReveals();
  startScenes();
};
