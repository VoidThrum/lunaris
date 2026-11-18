// The logo of a type of computer, by its connector's id: Lunaris' own, and
// those of the programs a computer of another type runs (RustDesk,
// Sunshine), shown only to say which kind of computer is meant. Their
// origin and licences: scripts/packaging/THIRD-PARTY-NOTICES.md. A type
// without a logo here is shown with its first letter.
import rustdesk from './assets/logos/rustdesk.svg';
import sunshine from './assets/logos/sunshine.svg';

const LOGOS: Record<string, string> = {
  lunaris: '/favicon.svg',
  rustdesk,
  sunshine,
};

/** The picture for the type `id` ("lunaris" or a connector's id), if it has one. */
export const typeLogo = (id: string): string | undefined =>
  (Object.prototype.hasOwnProperty.call(LOGOS, id) ? LOGOS[id] : undefined);
