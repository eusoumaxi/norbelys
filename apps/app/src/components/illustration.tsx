import { ILLUSTRATIONS } from "@brand/illustrations";
import type {
  Illustration as Drawing,
  IllustrationName,
} from "@brand/illustrations";
import { cn } from "cn";
import { createElement, useEffect, useMemo } from "react";

/** A shape's identity inside its drawing: a path by its outline, a circle by its geometry. */
const shapeKey = (
  tag: string,
  attributes: Readonly<Record<string, string | number>>
) =>
  `${tag}:${attributes.className ?? ""}:${attributes.d ?? `${attributes.cx},${attributes.cy},${attributes.r}`}`;

/**
 * One of the brand's single-line drawings, drawn from its data (no markup is injected). It draws
 * itself when it mounts (`nb-draw`); colours follow the theme through `--nb-illus-*`. It is
 * decoration: the text beside it says what it shows.
 *
 * With `once` (a key this browser remembers), it draws itself only the first time and appears
 * still afterwards: a drawing met on every visit (the setup card) should not replay its moment.
 */
export const Illustration = ({
  className,
  name,
  once,
}: {
  className?: string;
  name: IllustrationName;
  once?: string;
}) => {
  // Read once per mount: the effect below marks it drawn, for the next visit, not this one.
  const still = useMemo(() => {
    try {
      return once ? localStorage.getItem(once) === "1" : false;
    } catch {
      return false;
    }
  }, [once]);
  useEffect(() => {
    try {
      if (once) {
        localStorage.setItem(once, "1");
      }
    } catch {
      // Storage refused: it draws again next time, which is harmless.
    }
  }, [once]);
  const drawing: Drawing | undefined = ILLUSTRATIONS.find(
    (item) => item.name === name
  );
  if (!drawing) {
    return null;
  }
  return (
    <svg
      aria-hidden
      className={cn(
        "h-auto w-[200px] shrink-0",
        still ? null : "nb-draw",
        className
      )}
      fill="none"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth={2}
      viewBox="0 0 240 160"
    >
      {drawing.shapes.map(([tag, attributes]) =>
        createElement(tag, { ...attributes, key: shapeKey(tag, attributes) })
      )}
    </svg>
  );
};
