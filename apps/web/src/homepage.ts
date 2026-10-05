/**
 * The homepage's progressive enhancement. Without it every section is complete and readable;
 * with it the header behaves, sections reveal themselves and scroll scenes play.
 */
import { startHeader } from "./scripts/header";
import { startNotes, startParallax } from "./scripts/hero";
import { startMagnetic } from "./scripts/magnetic";
import { startMarquee } from "./scripts/marquee";
import { startMotion } from "./scripts/motion";

startHeader();
startMotion();
startNotes();
startParallax();
startMarquee();
startMagnetic();
