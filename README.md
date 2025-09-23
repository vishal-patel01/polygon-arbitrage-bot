# Polygon Arbitrage Opportunity Detector Bot

## Overview
This Rust bot detects potential arbitrage opportunities on the Polygon network. It periodically checks prices for token pairs (e.g., WETH/USDC) on multiple DEXes (QuickSwap, SushiSwap) via Polygon RPC. If a profitable difference is found (after simulated gas cost), it logs the opportunity and stores it in a SQLite database.

## Setup
1. Clone the repo: