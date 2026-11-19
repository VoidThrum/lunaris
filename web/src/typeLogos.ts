// The logo of a type of computer, by its connector's id: Lunaris' own, and
// those of the programs a computer of another type runs (RustDesk,
// Sunshine), shown only to say which kind of computer is meant. Their
// origin and licences: scripts/packaging/THIRD-PARTY-NOTICES.md. RDP and
// VNC are plain glyphs of Lunaris' own (a screen, two arrows, the letters),
// not any vendor's artwork. A type without a logo here is shown with its
// first letter.
import lunaris from './assets/logos/lunaris.svg';
import rustdesk from './assets/logos/rustdesk.svg';
import sunshine from './assets/logos/sunshine.svg';
import rdp from './assets/logos/rdp.svg';
import vnc from './assets/logos/vnc.svg';

const LOGOS: Record<string, string> = {
  lunaris,
  rustdesk,
  sunshine,
  rdp,
  vnc,
};

/** The picture for the type `id` ("lunaris" or a connector's id), if it has one. */
export const typeLogo = (id: string): string | undefined =>
  (Object.prototype.hasOwnProperty.call(LOGOS, id) ? LOGOS[id] : undefined);
