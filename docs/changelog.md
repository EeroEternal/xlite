# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- **xlite 初版**：独立仓库的轻量 OpenAI 网关。数据面只做 API Key 鉴权、按权重/优先级选通道、SSE 字节透传；h2c 走原生连接池（对端 `SETTINGS_MAX_CONCURRENT_STREAMS` 满了再拨新连接）。控制面只有供应商、模型与通道、API Key、设置。调度与 New API 相同：同一优先级内按权重随机，首字节前才 failover。
