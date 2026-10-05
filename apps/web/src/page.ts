/** Initializes the progressive enhancements present on each secondary page. */
import { startBinder } from "./scripts/binder";
import { startBounce } from "./scripts/bounce";
import { startBox } from "./scripts/box";
import { startContents } from "./scripts/contents";
import { startCopy } from "./scripts/copy";
import { startEnvelope } from "./scripts/envelope";
import { startFacts } from "./scripts/facts";
import { startHeader } from "./scripts/header";
import { startMotion } from "./scripts/motion";
import { startRoad } from "./scripts/road";
import { startShare } from "./scripts/share";
import { startTabs } from "./scripts/tabs";

startHeader();
startMotion();
startContents();
startShare();
startBinder();
startEnvelope();
startFacts();
startTabs();
startCopy();
startRoad();
startBox();
startBounce();
