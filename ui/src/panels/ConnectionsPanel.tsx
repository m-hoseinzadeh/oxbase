export function ConnectionsPanel() {
  return (
    <section className="panel panel--connections" aria-label="Connections">
      <div className="empty-state">
        <p className="empty-state__title">No connections yet</p>
        <p className="empty-state__hint">Saved database connections will appear here.</p>
      </div>
    </section>
  );
}
