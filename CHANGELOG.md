# Changelog

## [0.1.0](https://github.com/IndawoMaps/sabre/compare/v0.0.2...v0.1.0) (2026-09-30)


### ⚠ BREAKING CHANGES

* **core:** /query polygon results weight boundary pixels by coverage, so avg and stdev differ slightly from before; min, max, avg and stdev are null, not an error, when every covered pixel is nodata.

### Features

* **browser:** clip to geometry sent to the worker once ([94d8be9](https://github.com/IndawoMaps/sabre/commit/94d8be970a708b51721197fcac7d3b0841295b30))
* clip to geometry named by id in the browser ([f063972](https://github.com/IndawoMaps/sabre/commit/f0639726fb4cf612011232bee02c2aed021c27cb))
* **core:** hold clip geometry locally, named by provider and id ([54380c3](https://github.com/IndawoMaps/sabre/commit/54380c3720d99034985aad6142a2b3209caac101))
* **core:** zonal statistics weighted by exact pixel coverage ([fae8463](https://github.com/IndawoMaps/sabre/commit/fae846385ae1676fcc36fec25def6d5c19c045d7))
* **core:** zonal statistics weighted by exact pixel coverage ([86b6878](https://github.com/IndawoMaps/sabre/commit/86b6878705bfc88faa0beec4c5d12ab5225c1c75))
* **example:** clip the React Native example to fields by name ([3184a98](https://github.com/IndawoMaps/sabre/commit/3184a987ddf0c2014f7ba37fd71e1185a7b26be3))
* **mobile:** hold clip geometry in the app, named by provider and id ([711d8c2](https://github.com/IndawoMaps/sabre/commit/711d8c2d444136b0259e41df2ac34fcbb02e045c))
* **ol:** redraw a source when its named geometry changes ([9092291](https://github.com/IndawoMaps/sabre/commit/9092291f58ef2857ef0789efb5b46dc6e2c52725))
* **react-native:** clip to geometry put in once and named ([2b5a1f2](https://github.com/IndawoMaps/sabre/commit/2b5a1f2e16c583c1588ed8c2a9009da7b13a367d))
* **react-native:** clip to geometry put in once and named ([cb537bb](https://github.com/IndawoMaps/sabre/commit/cb537bbeb3dc9c2ae173b846f3edb0635f94871a))


### Bug Fixes

* **mobile:** restart a server that no longer answers ([0bbce49](https://github.com/IndawoMaps/sabre/commit/0bbce4965a0ef47f75c464bdffc717000397ae5f))


### Performance

* **core:** clip tiles and find zonal stats pixels by scanline ([967921c](https://github.com/IndawoMaps/sabre/commit/967921c1078c87849d6196a1ffd4b2cb31e0ae81))
* **core:** clip tiles by scanline in tile pixels ([d23fab3](https://github.com/IndawoMaps/sabre/commit/d23fab31e3536e2075d8ee950c539b16e0e7b048))
* **core:** find zonal statistics pixels by scanline ([de7597a](https://github.com/IndawoMaps/sabre/commit/de7597a57d6b1f5b7d7c67818bc31035e4eb753f))

## [0.0.2](https://github.com/IndawoMaps/sabre/compare/v0.0.1...v0.0.2) (2026-09-30)


### Features

* add @sabremaps/react-native, offline COGs on a MapLibre React Native map ([ec95d56](https://github.com/IndawoMaps/sabre/commit/ec95d56839cbe39c0e97ca7360659cef44ba85a1))
* **mobile:** run sabre's tile server inside a mobile app for MapLibre Native ([2eba7f8](https://github.com/IndawoMaps/sabre/commit/2eba7f8e6856a8ac7c54747a5fb099392f0264e6))
* **react-native:** add @sabremaps/react-native, offline COGs on a MapLibre RN map ([9b3966a](https://github.com/IndawoMaps/sabre/commit/9b3966adba37614ce229bb8c72a4e5b1da944f9f))
* **react-native:** run on iOS ([4cd105f](https://github.com/IndawoMaps/sabre/commit/4cd105f9fddf23f95ea5cb4032ceb76e29d3b492))


### Bug Fixes

* **mobile:** draw the server token from the OS's secure random source ([af1f107](https://github.com/IndawoMaps/sabre/commit/af1f107974b4ec3e7f0f8ef305a17a3998002746))
* **react-native:** refuse to replace an app's own network security config ([0b75cf8](https://github.com/IndawoMaps/sabre/commit/0b75cf8f217f9eace432b0e713c0b59fee4abd1e))
