// SPDX-License-Identifier: AGPL-3.0-or-later

import type { Usage } from "./usage";

export const REPORT_URL = "https://tel.koidrive.com/batch/";
export const PROJECT_TOKEN = "phc_zS5sUnZrgWmsxoiodya3UeRdNfBkk5Pvdpsqiii4XqZ8";
export const REPORT_SCHEMA = 1;
const HOUR_MS = 3_600_000;

export interface ServerBuild {
  version: string;
  protoMin: number;
  protoMax: number;
}

export interface ReportHour {
  hour: number;
  id: string;
  usage: Usage;
}

export function reportBatch(hours: ReportHour[], build: ServerBuild): unknown {
  return {
    api_key: PROJECT_TOKEN,
    historical_migration: false,
    batch: hours.map(({ hour, id, usage }) => {
      const properties: Record<string, unknown> = {
        distinct_id: id,
        $process_person_profile: false,
        $geoip_disable: true,
        schema: REPORT_SCHEMA,
        server_version: build.version,
        proto_min: build.protoMin,
        proto_max: build.protoMax,
      };
      for (const [key, count] of Object.entries(usage.counts)) properties["c." + key] = count;
      for (const [route, buckets] of Object.entries(usage.latency)) properties["h." + route] = buckets;
      return { event: "server_report", uuid: id, timestamp: new Date(hour * HOUR_MS).toISOString(), properties };
    }),
  };
}
