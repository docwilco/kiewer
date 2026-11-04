#!/usr/bin/env bash
# A hand deploy or a rollback of a Cloud Run service to a named image.
# CI does the routine deploys: main's run deploys its image to
# kiewer-staging, the run that cuts a Release deploys to kiewer. This is
# for the cases in between: a rollback to a kept Release, or a candidate
# put in front of traffic by hand. Either way the new revision takes all
# of the service's traffic, and the last line shows the service's URL,
# its latest ready revision and its traffic split.
#
# Usage: scripts/deploy-kiewer.sh <service> <Version|short-hash>
#   service: kiewer (production) or kiewer-staging
#   e.g. scripts/deploy-kiewer.sh kiewer 0.4.1
#
# Needs gcloud signed in to the project with run.services.update, and
# GCP_PROJECT_ID in the environment (the same value as the repository
# variable CI reads). The image is pulled by tag from Artifact Registry;
# a tag the release job has not made yet fails here instead of running
# the previous Release.
set -euo pipefail

service="${1:?usage: $0 <kiewer|kiewer-staging> <Version|short-hash>}"
tag="${2:?usage: $0 <kiewer|kiewer-staging> <Version|short-hash>}"
project="${GCP_PROJECT_ID:?set GCP_PROJECT_ID}"
region=europe-west4
image="$region-docker.pkg.dev/$project/kiewer/kiewer:${tag#v}"

case "$service" in
  kiewer|kiewer-staging) ;;
  *) echo "deploy: unknown service $service" >&2; exit 2 ;;
esac

gcloud run deploy "$service" --project "$project" --region "$region" \
  --image "$image" --quiet
# A Release candidate's --no-traffic keeps LATEST out of the production
# service's traffic until --to-latest puts it back, so without this a
# deploy made while a candidate is out serves no one (#93). After a plain
# Release it changes nothing. Staging never takes a candidate.
if [ "$service" = kiewer ]; then
  gcloud run services update-traffic "$service" --project "$project" --region "$region" \
    --to-latest --quiet
fi
# The traffic split, so every run shows where the traffic went.
gcloud run services describe "$service" --project "$project" --region "$region" \
  --format 'value(status.url, status.latestReadyRevisionName, status.traffic)'
