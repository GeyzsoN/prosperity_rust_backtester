PYTHON ?= python3
PIP ?= $(PYTHON) -m pip
UV ?= uv
IMAGE ?= rust-backtester:local
TRADER ?=
DAY ?=
PRODUCTS ?= summary
PERSIST ?= 0
FLAT ?= 0
CARRY ?= 0
ROUND ?= tutorial
CARGO_CMD ?= ./scripts/cargo_local.sh
DOCTOR_CMD ?= ./scripts/doctor_local.sh

.PHONY: help doctor build build-release test install install-pip install-uv install-uv-editable backtest tutorial submission round1-submission round2-submission round3-submission round4-submission round5-submission round6-submission round7-submission round8-submission round1 round2 round3 round4 round5 round6 round7 round8 run run-tutorial docker-build docker-smoke clean

RUN_ARGS = $(if $(TRADER),--trader $(TRADER),) $(if $(filter-out all,$(DAY)),$(if $(DAY),--day=$(DAY),),) --products $(PRODUCTS) $(if $(filter 1 true yes,$(PERSIST)),--persist,) $(if $(filter 1 true yes,$(FLAT)),--flat,) $(if $(filter 1 true yes,$(CARRY)),--carry,)
RUN_ARGS_SUBMISSION = $(if $(TRADER),--trader $(TRADER),) --products $(PRODUCTS) $(if $(filter 1 true yes,$(PERSIST)),--persist,) $(if $(filter 1 true yes,$(FLAT)),--flat,) $(if $(filter 1 true yes,$(CARRY)),--carry,)

help: ## Show available rust_backtester targets
	@grep -E '^[a-zA-Z0-9_-]+:.*## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*## "}; {printf "%-22s %s\n", $$1, $$2}'

doctor: ## Print local build diagnostics, especially for macOS execution-policy issues
	$(DOCTOR_CMD)

build: ## Build the debug Rust binary
	$(CARGO_CMD) build

build-release: ## Build the release Rust binary
	$(CARGO_CMD) build --release

test: ## Run Rust unit and integration tests
	$(CARGO_CMD) test

install: ## Install the CLI with cargo on macOS or inside WSL2
	$(CARGO_CMD) install --path .

install-pip: ## Build and install the package with pip from this directory
	$(PIP) install .

install-uv: ## Install the tool with uv from this directory
	$(UV) tool install --force .

install-uv-editable: ## Install the tool with uv in editable mode
	$(UV) tool install --force --editable .

backtest: ## Run the latest local trader against the bundled latest round
	$(CARGO_CMD) run -- $(RUN_ARGS)

tutorial: ## Run datasets/tutorial; set DAY=-2 or DAY=-1 to narrow it
	$(CARGO_CMD) run -- $(RUN_ARGS) --dataset tutorial

submission: ## Run the submission dataset for ROUND when a submission file is present
	$(CARGO_CMD) run -- $(RUN_ARGS_SUBMISSION) --dataset $(ROUND)-submission

round1-submission: ## Run the submission dataset for round1 when present
	$(CARGO_CMD) run -- $(RUN_ARGS_SUBMISSION) --dataset round1-submission

round2-submission: ## Run the submission dataset for round2 when present
	$(CARGO_CMD) run -- $(RUN_ARGS_SUBMISSION) --dataset round2-submission

round3-submission: ## Run the submission dataset for round3 when present
	$(CARGO_CMD) run -- $(RUN_ARGS_SUBMISSION) --dataset round3-submission

round4-submission: ## Run the submission dataset for round4 when present
	$(CARGO_CMD) run -- $(RUN_ARGS_SUBMISSION) --dataset round4-submission

round5-submission: ## Run the submission dataset for round5 when present
	$(CARGO_CMD) run -- $(RUN_ARGS_SUBMISSION) --dataset round5-submission

round6-submission: ## Run the submission dataset for round6 when present
	$(CARGO_CMD) run -- $(RUN_ARGS_SUBMISSION) --dataset round6-submission

round7-submission: ## Run the submission dataset for round7 when present
	$(CARGO_CMD) run -- $(RUN_ARGS_SUBMISSION) --dataset round7-submission

round8-submission: ## Run the submission dataset for round8 when present
	$(CARGO_CMD) run -- $(RUN_ARGS_SUBMISSION) --dataset round8-submission

round1: ## Run datasets/round1; omit DAY to run all datasets in that round
	$(CARGO_CMD) run -- $(RUN_ARGS) --dataset round1

round2: ## Run datasets/round2; omit DAY to run all datasets in that round
	$(CARGO_CMD) run -- $(RUN_ARGS) --dataset round2

round3: ## Run datasets/round3; omit DAY to run all datasets in that round
	$(CARGO_CMD) run -- $(RUN_ARGS) --dataset round3

round4: ## Run datasets/round4; omit DAY to run all datasets in that round
	$(CARGO_CMD) run -- $(RUN_ARGS) --dataset round4

round5: ## Run datasets/round5; omit DAY to run all datasets in that round
	$(CARGO_CMD) run -- $(RUN_ARGS) --dataset round5

round6: ## Run datasets/round6; omit DAY to run all datasets in that round
	$(CARGO_CMD) run -- $(RUN_ARGS) --dataset round6

round7: ## Run datasets/round7; omit DAY to run all datasets in that round
	$(CARGO_CMD) run -- $(RUN_ARGS) --dataset round7

round8: ## Run datasets/round8; omit DAY to run all datasets in that round
	$(CARGO_CMD) run -- $(RUN_ARGS) --dataset round8

run: ## Auto-pick the latest trader and default to the latest bundled dataset
	$(CARGO_CMD) run --

run-tutorial: ## Auto-pick the latest trader and run the bundled tutorial datasets
	$(CARGO_CMD) run -- --dataset tutorial

docker-build: ## Build the isolated Docker image
	docker build -t $(IMAGE) .

docker-smoke: ## Build the isolated Docker image and run its default command
	docker build -t $(IMAGE) .
	docker run --rm $(IMAGE)

clean: ## Remove build output and generated state
	cargo clean
	find runs -mindepth 1 ! -name .gitkeep -exec rm -rf {} +
	find . -type d -name __pycache__ -prune -exec rm -rf {} +
