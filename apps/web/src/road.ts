/**
 * The campaigns page's road: a campaign drawn as a drive. The road is the sequence, each overhead
 * sign is a step at its day, the roadside signs are the controls, the mailboxes are the people
 * it writes to, and a pink exit takes anyone who replies off the road.
 *
 * The scene is a handful of numbers in the world (metres: x across the road, y up, z ahead) and a
 * pinhole camera that drives along z. `frame()` turns a camera position into what the SVG draws.
 * The page draws the first frame at build time, so the road is there without a script; the
 * script (scripts/road.ts) draws every frame after that as the visitor scrolls.
 */

/** The lens: where the vanishing point sits on the stage, and the focal length, in pixels. */
export interface View {
  readonly cx: number;
  readonly cy: number;
  readonly f: number;
}

/** How high the camera rides, and the nearest distance it draws. */
const EYE = 1.55;
const NEAR = 0.35;
/** Far enough to meet the horizon. */
const FAR = 400;
/** Half the road's width, and the painted line down its middle. */
const ROAD = 2.3;
const DASH = { gap: 3, half: 0.06, length: 1.4 } as const;
/** Things farther than this fade out, and nearer than this fly past. */
const FADE_FAR = 44;
const FADE_RANGE = 14;

/** Where each step's overhead sign stands: two metres of road for every day of the campaign. */
const dayAt = (day: number): number => 7 + day * 2;

/** The steps of the example campaign, the day each one starts, and the icon on its sign. */
export const STEPS = [
  { day: 1, glyph: "campaigns", name: "First email" },
  { day: 4, glyph: "mail", name: "Follow-up" },
  { day: 10, glyph: "tracking", name: "New angle" },
  { day: 16, glyph: "meetings", name: "Last touch" },
] as const;

/**
 * The exit for replies: a lane that peels off the road's right edge and bends away on an arc, and
 * the sign hung over where it starts.
 */
const RAMP = { radius: 16, start: 47, sweep: 1.15, width: 2.4 } as const;
const EXIT = { x: 3.4, z: 51 } as const;

/**
 * Where the camera rests as the visitor scrolls: the start, six metres short of each step's sign,
 * and in sight of the exit, leaning towards it.
 */
export const STOPS: readonly number[] = [
  0,
  ...STEPS.map((step) => dayAt(step.day) - 6),
  40.5,
];
const LEAN = 1.2;

export type SignKind = "cap" | "gap" | "hours" | "start" | "stop" | "weekend";

/** One thing standing by the road: what it is, and where its foot is. */
export type Sprite =
  | {
      readonly kind: "gantry";
      readonly step: number;
      readonly x: number;
      readonly z: number;
    }
  | { readonly kind: "exit"; readonly x: number; readonly z: number }
  | { readonly kind: "mailbox"; readonly x: number; readonly z: number }
  | { readonly kind: SignKind; readonly x: number; readonly z: number };

/**
 * The roadside signs. The first two stand before the first gantry, so they're in the opening
 * picture; the rest stand well behind each gantry's posts, so no post crosses their words.
 */
const SIGNS: readonly Sprite[] = [
  { kind: "start", x: -2.75, z: 7.5 },
  { kind: "hours", x: 2.75, z: 7.5 },
  { kind: "weekend", x: -3.4, z: 20 },
  { kind: "cap", x: 3.4, z: 20.5 },
  { kind: "gap", x: 3.4, z: 31 },
  { kind: "weekend", x: -3.4, z: 33 },
  { kind: "stop", x: 3.2, z: 42 },
];

const GANTRIES: readonly Sprite[] = STEPS.map((step, index) => ({
  kind: "gantry",
  step: index,
  x: 0,
  z: dayAt(step.day),
}));

/** Mailboxes line both sides, kept clear of the signs, the gantries and the exit ramp. */
const MAILBOXES: readonly Sprite[] = Array.from({ length: 30 }, (_, index) => {
  const side = index % 2 === 0 ? -1 : 1;
  return { kind: "mailbox", x: side * 3.9, z: 6 + index * 2.6 } as const;
}).filter(
  (box) =>
    !(box.x > 0 && box.z > RAMP.start - 3) &&
    ![...SIGNS, ...GANTRIES].some(
      (thing) =>
        Math.abs(thing.z - box.z) < 2 &&
        (thing.kind === "gantry" || Math.sign(thing.x) === Math.sign(box.x))
    )
);

const EXIT_SIGN: Sprite = { kind: "exit", ...EXIT };

/** Everything by the road, farthest first, so nearer things are drawn over farther ones. */
export const SPRITES: readonly Sprite[] = [
  ...GANTRIES,
  EXIT_SIGN,
  ...SIGNS,
  ...MAILBOXES,
].toSorted((a, b) => b.z - a.z);

/** Everything the SVG draws for one camera position. */
export interface Frame {
  readonly horizon: number;
  readonly road: string;
  readonly edges: string;
  readonly dashes: string;
  readonly reflectors: string;
  readonly ramp: string;
  readonly rampEdges: string;
  readonly sprites: readonly {
    readonly transform: string;
    readonly opacity: number;
    readonly up: boolean;
  }[];
}

/** The camera: how far along the road it is, and how far it has leaned towards the exit. */
export interface Camera {
  readonly z: number;
  readonly x: number;
}

const round = (value: number): number => Math.round(value * 10) / 10;
const clamp = (value: number, low = 0, high = 1): number =>
  Math.min(high, Math.max(low, value));

/** Where a point in the world lands on the stage. */
const project = (
  camera: Camera,
  view: View,
  x: number,
  y: number,
  z: number
): string => {
  const depth = Math.max(z - camera.z, NEAR);
  return `${round(view.cx + (view.f * (x - camera.x)) / depth)} ${round(view.cy - (view.f * (y - EYE)) / depth)}`;
};

const quad = (
  camera: Camera,
  view: View,
  x0: number,
  x1: number,
  z0: number,
  z1: number
): string =>
  `M${project(camera, view, x0, 0, z0)}L${project(camera, view, x1, 0, z0)}L${project(camera, view, x1, 0, z1)}L${project(camera, view, x0, 0, z1)}Z`;

/**
 * A point on the exit ramp, `angle` radians round its bend: on its left edge, which leaves the road
 * where the ramp starts, or on its right edge, which opens out to the lane's full width.
 */
const rampAt = (
  angle: number,
  side: "left" | "right"
): readonly [number, number] => {
  const peel = clamp(angle / 0.32);
  const width = side === "left" ? 0 : RAMP.width * peel * peel * (3 - 2 * peel);
  const r = RAMP.radius - width;
  return [
    ROAD + RAMP.radius - r * Math.cos(angle),
    RAMP.start + r * Math.sin(angle),
  ];
};

export const frame = (camera: Camera, view: View): Frame => {
  const near = camera.z + NEAR;
  const dashes: string[] = [];
  for (
    let z = Math.ceil(near / DASH.gap) * DASH.gap - DASH.gap;
    z < camera.z + 70;
    z += DASH.gap
  ) {
    if (z + DASH.length > near) {
      dashes.push(
        quad(
          camera,
          view,
          -DASH.half,
          DASH.half,
          Math.max(z, near),
          z + DASH.length
        )
      );
    }
  }
  const reflectors: string[] = [];
  for (let z = Math.ceil(near / 2) * 2; z < camera.z + 46; z += 2) {
    const depth = z - camera.z;
    const r = round(clamp((view.f * 0.05) / depth, 0.6, 3.5));
    for (const side of [-1, 1]) {
      const [x = 0, y = 0] = project(
        camera,
        view,
        side * (ROAD + 0.22),
        0.04,
        z
      )
        .split(" ")
        .map(Number);
      reflectors.push(
        `M${round(x - r)} ${y}a${r} ${r} 0 1 0 ${round(r * 2)} 0a${r} ${r} 0 1 0 ${round(-r * 2)} 0`
      );
    }
  }
  // The ramp, sampled round its bend: its left edge out, then its right edge back.
  const angles = Array.from(
    { length: 25 },
    (_, index) => (index / 24) * RAMP.sweep
  ).filter((angle) => (rampAt(angle, "left")[1] ?? 0) > near);
  const edge = (side: "left" | "right"): string[] =>
    angles.map((angle) => {
      const [x, z] = rampAt(angle, side);
      return project(camera, view, x, 0, z);
    });
  const inner = edge("left");
  const outer = edge("right");
  const sprites = SPRITES.map((sprite) => {
    const depth = sprite.z - camera.z;
    const opacity =
      Math.round(
        clamp((FADE_FAR - depth) / FADE_RANGE) *
          clamp((depth - 0.5) / 0.7) *
          1000
      ) / 1000;
    if (opacity === 0) {
      return { opacity: 0, transform: "translate(-9999 -9999)", up: false };
    }
    // Art is drawn at 100 units to the metre, from the foot of its post.
    const scale = Math.round((view.f / depth / 100) * 10_000) / 10_000;
    return {
      opacity,
      transform: `translate(${project(camera, view, sprite.x, 0, sprite.z)}) scale(${scale})`,
      up: depth < 8,
    };
  });
  return {
    dashes: dashes.join(""),
    edges: [-ROAD, ROAD]
      .map(
        (x) =>
          `M${project(camera, view, x, 0, near)}L${project(camera, view, x, 0, FAR)}`
      )
      .join(""),
    horizon: round(view.cy),
    ramp:
      inner.length > 1
        ? `M${inner.join("L")}L${outer.toReversed().join("L")}Z`
        : "",
    rampEdges: inner.length > 1 ? `M${inner.join("L")}M${outer.join("L")}` : "",
    reflectors: reflectors.join(""),
    road: quad(camera, view, -ROAD, ROAD, near, FAR),
    sprites,
  };
};

/**
 * Where the camera is for a position along the stops (0 at the start, 1 at the first step's
 * sign...): it rests at each stop for a while and glides between them, then leans towards the exit
 * on the last stretch.
 */
export const cameraAt = (position: number): Camera => {
  const last = STOPS.length - 1;
  const at = clamp(position, 0, last);
  const index = Math.min(last - 1, Math.floor(at));
  const rest = 0.2;
  const t = clamp((at - index - rest) / (1 - rest * 2));
  const eased = t * t * t * (t * (t * 6 - 15) + 10);
  const from = STOPS[index] ?? 0;
  const to = STOPS[index + 1] ?? from;
  return {
    x: index === last - 1 ? LEAN * eased : 0,
    z: from + (to - from) * eased,
  };
};
