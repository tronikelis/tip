# Tronikel's interactive process

![main_demo](assets/main.gif)

Or `tip` for short.

A simple cli utility to interactively run other tools, e.g `jq`

## Usage

```
Usage: tip <program> [arguments]
```

## Install

To install you will have to build from source,
therefore a rust toolchain is required

```
git clone https://github.com/tronikelis/tip
cd tip && cargo install --path .
```

## Demo

<details>
    <summary>curl ... | tip jq</summary>
    <img src="assets/jq.gif" />
</details>

<details>
    <summary>tip rg</summary>
    <img src="assets/rg.gif" />
</details>
