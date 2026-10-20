import { FindZone } from "./FindZone";
import { ZoneTracks } from "./ZoneTracks";

/** Each thread's zones over the selected range, and beside them, Find
 * Zone, which measures one site's zones. */
export function ZonesView() {
  return (
    <div className="profile-zones" data-testid="zones">
      <ZoneTracks />
      <FindZone />
    </div>
  );
}
