FROM rayproject/ray:2.37.0.cabc24-py312

USER ray

RUN sudo apt update && \
    sudo apt install -y curl build-essential protobuf-compiler

# Intall Rust
RUN curl https://sh.rustup.rs -sSf | sh -s -- --default-toolchain stable -y

WORKDIR /home/ray/datafusion_ray

# Copy requirements file and install dependencies
COPY requirements-in.txt /home/ray/datafusion_ray/requirements-in.txt
RUN pip3 install -r /home/ray/datafusion_ray/requirements-in.txt

# Copy the rest of the source code
COPY . /home/ray/datafusion_ray

# Build within container
RUN sudo chown -R ray:users /home/ray/datafusion_ray && \
    source /home/ray/.cargo/env && \
    maturin build --release

# Copy built wheel to target container

FROM rayproject/ray:2.37.0.cabc24-py312
COPY --from=0 /home/ray/datafusion_ray/target/wheels/*.whl /home/ray/
RUN pip3 install /home/ray/*.whl
