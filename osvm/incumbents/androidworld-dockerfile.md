# AndroidWorld @55ddaf5 Dockerfile: the base image openjdk:18-jdk-slim is gone from Docker Hub (deprecated official image).
# T5 builds with this one-line change and nothing else: sed "s#^FROM openjdk:18-jdk-slim#FROM eclipse-temurin:17-jdk-jammy#" Dockerfile > Dockerfile.t5
