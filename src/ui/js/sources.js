import { sentence } from './format.js';
import { data } from './store.js';

// Indexer, IRC and Plex problems, shared by the overview and the settings.

export const INDEXER_ERRORS = {
  authentication_rejected: 'The key or login was refused', rate_limited: 'Too many searches; waiting', response_rejected: 'The indexer answered with an error',
  parse_rejected: 'The answer could not be read', request_failed: 'Could not be reached', protection_unavailable: 'Its network protection is not ready',
  policy_changed: 'Its settings changed', disabled: 'Turned off',
};
export const plexError = (text) => String(text).replace(/^Plex:\s*/, '');
export const indexerReports = () => data.get('indexers')?.data?.sources ?? [];
/** Indexer and IRC problems, for the overview. */
export function sourceProblems() {
  const indexers = indexerReports().filter((r) => r.last_error && r.last_error !== 'disabled')
    .map((r) => `Indexer ${r.name ?? r.id}: ${INDEXER_ERRORS[r.last_error] ?? sentence(r.last_error)}`);
  const irc = (data.get('irc')?.data?.sources ?? []).filter((r) => r.last_error ?? r.error)
    .map((r) => `IRC ${r.name ?? r.id}: ${r.last_error ?? r.error}`);
  return [...indexers, ...irc];
}
