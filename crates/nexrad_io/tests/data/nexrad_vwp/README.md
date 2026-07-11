# NEXRAD Product 48 fixture

`KBMX_SDUS54_NVWBMX_199804160006` is an unmodified NEXRAD Level III VAD
Wind Profile (Product 48) file from the public NOAA/NCEI Level III archive:

- source archive: `NWS_NEXRAD_NXL3_KBMX_19980416000000_19980416235959.tar.Z`
- radar: KBMX (Birmingham, Alabama)
- nominal volume time: 1998-04-16 00:06:45 UTC
- size: 7,090 bytes
- SHA-256: `c163d010f48492ce9bf5208ee6940c637a0905b8997666b0411112734d2abeb6`

It exercises the symbology fallback, metadata pages, and a rolling profile
timeline that crosses midnight. The latter makes it a regression fixture for
absolute HHMM ordering and the NEXRAD Julian-date epoch.
