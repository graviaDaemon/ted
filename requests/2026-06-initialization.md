# Initialization

Currently this application has been running on a live server for a couple of months and has made absolutely 0 progress in earning any actual money.
So we're going to overhaul this application in a few ways.

## Application Purpose
The application T.E.D is meant to be a fully autonomous trading bot that can run on a crypto market like bitfinex (or other such markets).
It should have an intuitive terminal interface where we can select algorithms, edit parameters, etc.

## Architecture
For the drawing board phase, I reccomend we start defining some of the back-end, middleware, and front-end features and work out the architecture from there.

## Must-Haves
Within the confines of whatever architecture we're going to build, I have some features I think are a must-have:
- API public/authenticated url and key should be configurable
- It should be able to run multiple coins at the same time, even on multiple markets if the user so wishes
- We should have a log of what the coins have been doing stored in a database, so we can create a seperate web interface overview
  - Note that we only have limited storage space, so we'd have to ensure this is not an ever-growing database
- There should be a logging system that logs to files somewhere, configurable by level (trace, debug, information, warning, error, critical)
- The algorithms we'd use are either hot-load, or there has to be a simple rebuild feature in the application that can pick up where it left off after rebuild
- The application will run on a linux device using tmux attach ted to get into the application's core

Let's build some questions, and figure out the architecture. Consider the planning as if there hasn't been an application just yet.
One of the main purposes is for this trading bot to make money with crypto. I am fully aware of the risks, and the responsibility for public API tokens and the actual money is mine
Yours is simply to make a trading bot that works and actually starts earning on the markets.