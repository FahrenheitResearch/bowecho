# IMGW-PIB POLRAD Cartesian ODIM fixtures

These four files are unmodified operational downloads from the official
IMGW-PIB public datastore on 2026-07-11 UTC. They are one Ramża scan's four
available dual-polarization CMAX products:

- `2026071100150601KDP.max.h5`
- `2026071100150601PhiDP.max.h5`
- `2026071100150601RhoHV.max.h5`
- `2026071100150601ZDR.max.h5`

Source directory:

`https://danepubliczne.imgw.pl/pl/datastore?product=HVD_ram_250.max`

The portal's file-list endpoint resolves each name under:

`https://danepubliczne.imgw.pl/pl/datastore/getfiledown/Oper/Polrad/Produkty/HVD/HVD_ram_250.max/`

Each compact file is ODIM_H5 `IMAGE`, version `H5rd 2.3`. `dataset1` is the
500x500 spherical-AEQD `MAX` grid; `dataset2`/`dataset3` are the VSP/HSP side
maximum projections. IMGW places `product`, `quantity`, `gain`, `offset`,
`nodata`, and `undetect` on `dataset1/what`, not `dataset1/data1/what`.

SHA-256:

- KDP: `18D05C3E75F337D2F96466BE3E9BE7DE9736EFD807D6D83809CFE58480C127AF`
- PhiDP: `3DDCA554DC250FDA09ABA9C7130646DB9BCF96C9634F5E2A8E3A0F6DA325DE2D`
- RhoHV: `9758A4F7AF62E0FA116E95088D22EF50123DD38DADDE3381C2670F8D05ACECD3`
- ZDR: `F65EDFA5EE3BB7B37E7CF937750A20142661051D4E5961A925C401CA07FFDA11`

The datastore terms require source attribution to Instytut Meteorologii i
Gospodarki Wodnej – Państwowy Instytut Badawczy and disclosure when its data
has been processed. These fixtures remain test inputs and are not modified.
